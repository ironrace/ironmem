//! Hand-computed 20-row fixture for SPEC §7.1 + §9.2 metrics math.
//!
//! Layout (see Task 9 acceptance criteria):
//!   - 10 GT=Valid: 9 → "valid", 1 → "stale" → valid_retention = 0.9
//!   - 8 GT=Stale*: 6 → "stale", 2 → "valid" → recall = 6/8 = 0.75, precision = 6/7 ≈ 0.857
//!   - 2 GT=NeedsRevalidation, both → "needs_revalidation" → routing = 1.0
//!
//! Stale subtype breakdown (8 rows total):
//!   - 5 StaleSourceChanged: 4 → "stale", 1 → "valid"
//!   - 2 StaleSourceDeleted: 2 → "stale", 0 → "valid"
//!   - 1 StaleSymbolRenamed: 0 → "stale", 1 → "valid"

use provbench_baseline::metrics::{
    coalesce, cost_per_correct_invalidation_from_totals, latency, llm_validator_agreement,
    three_way, wilson_lower_95,
};
use provbench_baseline::runner::PredictionRow;
use std::collections::HashMap;

fn row(
    fact_id: &str,
    commit: &str,
    batch: &str,
    gt: &str,
    pred: &str,
    wall_ms: u64,
) -> PredictionRow {
    PredictionRow {
        fact_id: fact_id.to_string(),
        commit_sha: commit.to_string(),
        batch_id: batch.to_string(),
        ground_truth: gt.to_string(),
        prediction: pred.to_string(),
        request_id: "fixture".to_string(),
        wall_ms,
        wall_us: None,
        evidence: None,
        row_index: None,
    }
}

fn fixture_20_rows() -> Vec<PredictionRow> {
    let mut rows = Vec::new();
    // 10 GT=Valid: 9 predict "valid", 1 predicts "stale".
    for i in 0..9 {
        rows.push(row(
            &format!("v{i}"),
            "c-v0",
            "c-v0-0",
            "Valid",
            "valid",
            100,
        ));
    }
    rows.push(row("v9", "c-v1", "c-v1-0", "Valid", "stale", 200));

    // 5 GT=StaleSourceChanged: 4 predict "stale", 1 predicts "valid".
    for i in 0..4 {
        rows.push(row(
            &format!("sc{i}"),
            "c-sc",
            "c-sc-0",
            "StaleSourceChanged",
            "stale",
            300,
        ));
    }
    rows.push(row(
        "sc4",
        "c-sc",
        "c-sc-0",
        "StaleSourceChanged",
        "valid",
        300,
    ));

    // 2 GT=StaleSourceDeleted: both predict "stale".
    for i in 0..2 {
        rows.push(row(
            &format!("sd{i}"),
            "c-sd",
            "c-sd-0",
            "StaleSourceDeleted",
            "stale",
            400,
        ));
    }

    // 1 GT=StaleSymbolRenamed: predicts "valid".
    rows.push(row(
        "sr0",
        "c-sr",
        "c-sr-0",
        "StaleSymbolRenamed",
        "valid",
        500,
    ));

    // 2 GT=NeedsRevalidation: both predict "needs_revalidation".
    rows.push(row(
        "n0",
        "c-n",
        "c-n-0",
        "NeedsRevalidation",
        "needs_revalidation",
        600,
    ));
    rows.push(row(
        "n1",
        "c-n",
        "c-n-0",
        "NeedsRevalidation",
        "needs_revalidation",
        600,
    ));
    rows
}

fn uniform_weights() -> HashMap<String, f64> {
    let mut w = HashMap::new();
    w.insert("valid".to_string(), 1.0 / 3.0);
    w.insert("stale".to_string(), 1.0 / 3.0);
    w.insert("needs_revalidation".to_string(), 1.0 / 3.0);
    w
}

#[test]
fn three_way_matches_hand_computed() {
    let rows = fixture_20_rows();
    let w = uniform_weights();
    let r = three_way(&rows, &w);

    // Stale: TP=6, FN=2, FP=1.
    let recall = 6.0 / 8.0;
    let precision = 6.0 / 7.0;
    let f1 = 2.0 * precision * recall / (precision + recall);
    assert!(
        (r.stale_detection.recall - recall).abs() < 1e-3,
        "recall: got {}, want {}",
        r.stale_detection.recall,
        recall
    );
    assert!(
        (r.stale_detection.precision - precision).abs() < 1e-3,
        "precision: got {}, want {}",
        r.stale_detection.precision,
        precision
    );
    assert!(
        (r.stale_detection.f1 - f1).abs() < 1e-3,
        "f1: got {}, want {}",
        r.stale_detection.f1,
        f1
    );

    // Valid retention = 9/10 = 0.9.
    assert!(
        (r.valid_retention_accuracy.point - 0.9).abs() < 1e-3,
        "valid retention: got {}",
        r.valid_retention_accuracy.point
    );
    // NR routing = 2/2 = 1.0.
    assert!(
        (r.needs_revalidation_routing_accuracy.point - 1.0).abs() < 1e-3,
        "nr routing: got {}",
        r.needs_revalidation_routing_accuracy.point
    );

    // Wilson lower bounds: sanity (0..=point).
    assert!(r.stale_detection.wilson_lower_95 <= recall);
    assert!(r.stale_detection.wilson_lower_95 >= 0.0);
    assert!(r.valid_retention_accuracy.wilson_lower_95 <= 0.9);
    // 2-of-2 Wilson lower: (1 + z²/4 - z*sqrt((0 + z²/8)/2)) / (1 + z²/2)
    // ≈ (1 + 0.96 - 1.96*sqrt(0.48/2)) / (1 + 1.92) ≈ (1.96 - 0.96)/2.92 ≈ 0.342
    let nr_lo = r.needs_revalidation_routing_accuracy.wilson_lower_95;
    assert!(
        nr_lo > 0.30 && nr_lo < 0.40,
        "nr Wilson lower out of bounds: {nr_lo}"
    );
}

#[test]
fn agreement_matches_hand_computed() {
    let rows = fixture_20_rows();
    let w = uniform_weights();
    let a = llm_validator_agreement(&rows, &w);

    // Confusion matrix:
    //   GT=valid: 9 valid, 1 stale, 0 nr → [9, 1, 0]
    //   GT=stale: 2 valid, 6 stale, 0 nr → [2, 6, 0]
    //   GT=nr:    0 valid, 0 stale, 2 nr → [0, 0, 2]
    assert_eq!(a.confusion_matrix_3x3.len(), 3);
    assert_eq!(a.confusion_matrix_3x3[0].len(), 3);
    assert_eq!(a.confusion_matrix_3x3[0], [9, 1, 0]);
    assert_eq!(a.confusion_matrix_3x3[1], [2, 6, 0]);
    assert_eq!(a.confusion_matrix_3x3[2], [0, 0, 2]);

    // Unweighted overall would be 17/20 = 0.85. HT-weighted with uniform
    // weights renormalised over present classes (all 3 present, equal
    // share 1/3): overall = (1/3)(9/10) + (1/3)(6/8) + (1/3)(2/2)
    //                     = (1/3)(0.9 + 0.75 + 1.0) = 2.65/3 ≈ 0.8833.
    let expected_overall = (0.9 + 0.75 + 1.0) / 3.0;
    assert!(
        (a.overall.point - expected_overall).abs() < 1e-3,
        "overall: got {}, want {}",
        a.overall.point,
        expected_overall
    );

    // Per-class agreement: valid 9/10, stale 6/8, nr 2/2.
    assert!((a.per_class.get("valid").copied().unwrap_or(-1.0) - 0.9).abs() < 1e-3);
    assert!((a.per_class.get("stale").copied().unwrap_or(-1.0) - 0.75).abs() < 1e-3);
    assert!(
        (a.per_class
            .get("needs_revalidation")
            .copied()
            .unwrap_or(-1.0)
            - 1.0)
            .abs()
            < 1e-3
    );

    // Cohen κ: p_o = 0.85, p_e = (10/20)(11/20) + (8/20)(7/20) + (2/20)(2/20)
    //                          = 0.275 + 0.14 + 0.01 = 0.425.
    // κ = (0.85 - 0.425)/(1 - 0.425) = 0.425/0.575 ≈ 0.7391.
    let expected_kappa = (0.85 - 0.425) / (1.0 - 0.425);
    assert!(
        (a.cohen_kappa.point_estimate - expected_kappa).abs() < 1e-3,
        "kappa: got {}, want {}",
        a.cohen_kappa.point_estimate,
        expected_kappa
    );
    assert!(a.cohen_kappa.point_estimate.abs() <= 1.0);
    assert!(a.cohen_kappa.ci_95_lower <= a.cohen_kappa.point_estimate);
    assert!(a.cohen_kappa.ci_95_upper >= a.cohen_kappa.point_estimate);
    assert_eq!(a.cohen_kappa.n_bootstrap, 1000);

    // Per-stale subtype:
    //   changed: 4/5 = 0.8
    //   deleted: 2/2 = 1.0
    //   renamed: 0/1 = 0.0
    assert!((a.per_stale_subtype.get("changed").copied().unwrap_or(-1.0) - 0.8).abs() < 1e-3);
    assert!((a.per_stale_subtype.get("deleted").copied().unwrap_or(-1.0) - 1.0).abs() < 1e-3);
    assert!((a.per_stale_subtype.get("renamed").copied().unwrap_or(-1.0)).abs() < 1e-3);
}

#[test]
fn latency_groups_by_commit_dedups_batches() {
    let rows = fixture_20_rows();
    // Commits and their batch wall_ms:
    //   c-v0 : 1 batch @ 100 ms
    //   c-v1 : 1 batch @ 200 ms
    //   c-sc : 1 batch @ 300 ms (5 rows share it)
    //   c-sd : 1 batch @ 400 ms
    //   c-sr : 1 batch @ 500 ms
    //   c-n  : 1 batch @ 600 ms
    // Per-commit totals sorted: [100, 200, 300, 400, 500, 600] (n=6)
    let l = latency(&rows);
    // p50 nearest-rank: ceil(0.5 * 6) = 3 → idx 2 → 300.
    assert_eq!(l.p50_ms, 300);
    // p95 nearest-rank: ceil(0.95 * 6) = 6 → idx 5 → 600.
    assert_eq!(l.p95_ms, 600);
}

#[test]
fn cost_per_correct_invalidation_divides_by_tp_stale() {
    let rows = fixture_20_rows();
    // TP-stale = 6 (4 changed + 2 deleted predicted stale).
    let c = cost_per_correct_invalidation_from_totals(&rows, 600, 12.0);
    assert!((c.usd - 2.0).abs() < 1e-9, "usd per correct: got {}", c.usd);
    assert_eq!(c.tokens, 100);
}

#[test]
fn coalesce_maps_all_raw_tags() {
    assert_eq!(coalesce("Valid"), "valid");
    assert_eq!(coalesce("StaleSourceChanged"), "stale");
    assert_eq!(coalesce("StaleSourceDeleted"), "stale");
    assert_eq!(coalesce("StaleSymbolRenamed"), "stale");
    assert_eq!(coalesce("NeedsRevalidation"), "needs_revalidation");
    // Already-coalesced model outputs also pass through.
    assert_eq!(coalesce("valid"), "valid");
    assert_eq!(coalesce("stale"), "stale");
    assert_eq!(coalesce("needs_revalidation"), "needs_revalidation");
    // Spotcheck/export paths may use snake_case subtype labels.
    assert_eq!(coalesce("stale_source_changed"), "stale");
    assert_eq!(coalesce("stale_source_deleted"), "stale");
    assert_eq!(coalesce("stale_symbol_renamed"), "stale");
}

#[test]
fn wilson_edges() {
    // 0/0 → 0.0 (safe sentinel).
    assert_eq!(wilson_lower_95(0, 0), 0.0);
    // n/n is strictly below 1.0.
    assert!(wilson_lower_95(10, 10) < 1.0);
    assert!(wilson_lower_95(10, 10) > 0.6);
}

#[test]
fn overall_agreement_is_17_over_20_unweighted() {
    // Spec line in the test plan: overall agreement = 17/20 = 0.85.
    // That is the *unweighted* overall — verify the agreement function's
    // unweighted fallback returns it when weights are empty.
    let rows = fixture_20_rows();
    let empty: HashMap<String, f64> = HashMap::new();
    let a = llm_validator_agreement(&rows, &empty);
    assert!(
        (a.overall.point - 0.85).abs() < 1e-3,
        "unweighted overall: got {}",
        a.overall.point
    );
}
