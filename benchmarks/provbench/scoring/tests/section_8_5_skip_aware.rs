//! SPEC §8 #5 SKIP-awareness: when ground-truth stale_* count == 0,
//! `section_8_5_stale_recall_wlb_ge_0_30` MUST be recorded as
//! `status: "SKIP"` rather than `passed: false`. See SPEC §11 row
//! 2026-05-18 (round v1.2c).

use std::fs;
use tempfile::TempDir;

/// Minimal `metrics.json` the baseline dir must contain so `compare::run`
/// can load it.  Fields that `compare::run` does not read default to 0 /
/// empty, which is fine for a RED-phase acceptance test.
fn minimal_baseline_metrics() -> serde_json::Value {
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

/// Build a predictions.jsonl line in the *real* PredictionRow schema.
///
/// Field mapping from the spec synthetic data → actual schema:
///   row_id     → fact_id  (prefixed with "r")
///   decision   → prediction
///   ground_truth is unchanged
///   commit_sha, batch_id, request_id are synthetic constants
///   rule_id is NOT a PredictionRow field — dropped
fn pred_line(i: usize, prediction: &str, ground_truth: &str) -> String {
    serde_json::json!({
        "fact_id":      format!("r{i}"),
        "commit_sha":   "0000000000",
        "batch_id":     "test-batch",
        "ground_truth": ground_truth,
        "prediction":   prediction,
        "request_id":   format!("phase1:v1.2c:0000000000:{i}"),
        "wall_ms":      1_u64
    })
    .to_string()
}

fn write_predictions(dir: &std::path::Path, lines: &[String]) {
    fs::write(dir.join("predictions.jsonl"), lines.join("\n")).unwrap();
}

// ---------------------------------------------------------------------------
// Test 1: SKIP when ground-truth stale_* count == 0
// ---------------------------------------------------------------------------

#[test]
fn section_8_5_records_skip_when_no_stale_ground_truth() {
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

    // Synthetic predictions: 100 rows, GT = 50 valid + 50 needs_revalidation,
    // 0 stale_*.  Candidate predicts perfectly.  Baseline uses the same rows.
    let preds: Vec<String> = (0..100)
        .map(|i| {
            let class = if i < 50 {
                "valid"
            } else {
                "needs_revalidation"
            };
            pred_line(i, class, class)
        })
        .collect();

    write_predictions(&candidate, &preds);
    write_predictions(&baseline, &preds);

    let report = provbench_scoring::compare::run(&baseline, &candidate, "phase1_rules").unwrap();

    let s8_5 = &report["thresholds"]["section_8_5_stale_recall_wlb_ge_0_30"];
    assert_eq!(
        s8_5["status"].as_str(),
        Some("SKIP"),
        "expected SKIP when GT stale_* count == 0, got: {s8_5}"
    );
    assert!(s8_5["passed"].is_null(), "passed must be null on SKIP");
    let reason = s8_5["reason"].as_str().unwrap_or("");
    assert!(
        reason.contains("ground_truth_stale_count"),
        "reason must name the trigger; got: {reason}"
    );
}

// ---------------------------------------------------------------------------
// Test 2: PASS (or FAIL) when stale ground-truth rows ARE present
// ---------------------------------------------------------------------------

#[test]
fn section_8_5_records_pass_or_fail_when_stale_ground_truth_present() {
    let tmp = TempDir::new().unwrap();
    let candidate = tmp.path().join("candidate");
    let baseline = tmp.path().join("baseline");
    fs::create_dir_all(&candidate).unwrap();
    fs::create_dir_all(&baseline).unwrap();

    fs::write(
        baseline.join("metrics.json"),
        serde_json::to_string(&minimal_baseline_metrics()).unwrap(),
    )
    .unwrap();

    // GT includes 50 stale rows so the threshold is evaluable.
    // Candidate predicts perfectly → WLB will be > 0.30 → PASS.
    let preds: Vec<String> = (0..100)
        .map(|i| {
            let (pred, gt) = if i < 50 {
                ("stale", "stale")
            } else {
                ("valid", "valid")
            };
            pred_line(i, pred, gt)
        })
        .collect();

    write_predictions(&candidate, &preds);
    write_predictions(&baseline, &preds);

    let report = provbench_scoring::compare::run(&baseline, &candidate, "phase1_rules").unwrap();

    let s8_5 = &report["thresholds"]["section_8_5_stale_recall_wlb_ge_0_30"];
    assert_eq!(
        s8_5["status"].as_str(),
        Some("PASS"),
        "expected PASS when stale rows present and perfectly recalled, got: {s8_5}"
    );
    assert_eq!(
        s8_5["passed"].as_bool(),
        Some(true),
        "passed must be true when stale GT present and perfectly recalled, got: {s8_5}"
    );
}

// ---------------------------------------------------------------------------
// Test 3: Fix 1 regression — PascalCase labeler tags must coalesce to stale
// ---------------------------------------------------------------------------

fn write_minimal_baseline_metrics(dir: &std::path::Path) {
    fs::write(
        dir.join("metrics.json"),
        serde_json::to_string(&minimal_baseline_metrics()).unwrap(),
    )
    .unwrap();
}

#[test]
fn section_8_5_handles_pascalcase_labeler_stale_tags() {
    let tmp = TempDir::new().unwrap();
    let candidate = tmp.path().join("candidate");
    let baseline = tmp.path().join("baseline");
    fs::create_dir_all(&candidate).unwrap();
    fs::create_dir_all(&baseline).unwrap();

    // Synth: 50 valid + 50 PascalCase StaleSourceChanged GT rows.
    // The candidate predicts coalesced lowercase "stale" / "valid".
    // count_ground_truth_stale MUST detect the PascalCase rows as stale.
    let mut preds = String::new();
    for i in 0..50 {
        preds.push_str(&pred_line(i, "valid", "valid"));
        preds.push('\n');
    }
    for i in 50..100 {
        preds.push_str(&pred_line(i, "stale", "StaleSourceChanged"));
        preds.push('\n');
    }

    fs::write(candidate.join("predictions.jsonl"), &preds).unwrap();
    fs::write(baseline.join("predictions.jsonl"), &preds).unwrap();
    write_minimal_baseline_metrics(&baseline);

    let report = provbench_scoring::compare::run(&baseline, &candidate, "phase1_rules").unwrap();
    let s8_5 = &report["thresholds"]["section_8_5_stale_recall_wlb_ge_0_30"];

    // PascalCase MUST be recognized as stale, so status is PASS/FAIL, not SKIP.
    assert_ne!(
        s8_5["status"].as_str(),
        Some("SKIP"),
        "PascalCase StaleSourceChanged tags must coalesce to CLASS_STALE; got: {s8_5}"
    );
}
