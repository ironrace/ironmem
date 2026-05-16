//! SPEC §9.4 held-out gate on pallets/flask @ 2f0c62f5.
//!
//! Asserts the three §8 thresholds verbatim against the
//! `phase1_rules` column in the held-out canary metrics. Also
//! asserts row-count consistency (== baseline `selected_count`) and
//! `rule_set_version=v1.2` evidence in every prediction's
//! `request_id`.
//!
//! Unlike the serde Round 1 test, this test does NOT re-run the
//! phase1 scorer + provbench-score compare — the recorded
//! `<RUNDIR>/metrics.json` is the contract. The test reads it
//! directly so that the §8 #5 stale-recall FAIL is preserved
//! verbatim from the actual round artifacts.
//!
//! On §8 miss this test fails honestly. Per SPEC §10 the round
//! does NOT retune in response — the failure is recorded as the
//! held-out result in `results/flask-heldout-2026-05-15-findings.md`
//! and SPEC §11. **A FAIL here is the recorded experimental result,
//! NOT a regression to fix.**
//!
//! Expected outcome on the v1.2b flask canary:
//! - §8 #3 valid retention WLB ~0.9981 >= 0.95  → PASS
//! - §8 #4 latency p50 0 ms <= 727 ms           → PASS
//! - §8 #5 stale recall WLB 0.0 < 0.30          → FAIL (recorded)
//!
//! The §8 #5 FAIL is structural for this round: the Plan A.1 labeler
//! emits zero Stale_* ground truth on the flask corpus because all
//! changed Python files route to NeedsRevalidation via the
//! PR #52 short-circuit. See the Task 10 findings doc for the full
//! taxonomy-mismatch narrative.
//!
//! ## Running
//!
//! Invocation (the `#[ignore]` attribute requires `--ignored`):
//! ```text
//! cargo test --release --manifest-path benchmarks/provbench/phase1/Cargo.toml \
//!     --test end_to_end_heldout_flask -- --ignored --nocapture
//! ```

use std::path::PathBuf;

const HELDOUT_RUN_DIR: &str = "results/flask-heldout-2026-05-15-canary";
const EXPECTED_RULE_SET_VERSION: &str = "v1.2";
const EXPECTED_SUBSET_SIZE: u64 = 4000;

fn provbench_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn run_dir() -> PathBuf {
    provbench_root().join(HELDOUT_RUN_DIR)
}

#[test]
#[ignore = "asserts recorded held-out flask metrics.json verbatim; \
            §8 #5 fails honestly on this round — run with --ignored"]
fn spec_section_8_3_valid_retention_on_flask_heldout() {
    let metrics_path = run_dir().join("metrics.json");
    let metrics: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&metrics_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", metrics_path.display())),
    )
    .expect("parse metrics.json");

    let valid_wlb = metrics["phase1_rules"]["section_7_1"]["valid_retention_accuracy"]
        ["wilson_lower_95"]
        .as_f64()
        .expect("phase1_rules valid_retention_accuracy wilson_lower_95");

    assert!(
        valid_wlb >= 0.95,
        "§8 #3 valid retention WLB {:.4} < 0.95",
        valid_wlb
    );
}

#[test]
#[ignore = "asserts recorded held-out flask metrics.json verbatim; \
            §8 #5 fails honestly on this round — run with --ignored"]
fn spec_section_8_4_latency_p50_on_flask_heldout() {
    let metrics_path = run_dir().join("metrics.json");
    let metrics: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&metrics_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", metrics_path.display())),
    )
    .expect("parse metrics.json");

    let p50 = metrics["phase1_rules"]["section_7_2_applicable"]["latency_p50_ms"]
        .as_u64()
        .expect("phase1_rules latency_p50_ms");

    assert!(p50 <= 727, "§8 #4 latency p50 {} ms > 727", p50);
}

#[test]
#[ignore = "asserts recorded held-out flask metrics.json verbatim; \
            §8 #5 fails honestly on this round — run with --ignored"]
fn spec_section_8_5_stale_recall_on_flask_heldout() {
    let metrics_path = run_dir().join("metrics.json");
    let metrics: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&metrics_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", metrics_path.display())),
    )
    .expect("parse metrics.json");

    let stale_wlb = metrics["phase1_rules"]["section_7_1"]["stale_detection"]["wilson_lower_95"]
        .as_f64()
        .expect("phase1_rules stale_detection wilson_lower_95");

    assert!(
        stale_wlb >= 0.30,
        "§8 #5 stale recall WLB {:.4} < 0.30",
        stale_wlb
    );
}

#[test]
#[ignore = "asserts recorded held-out flask metrics.json verbatim; \
            §8 #5 fails honestly on this round — run with --ignored"]
fn row_count_matches_baseline_subset_size_on_flask_heldout() {
    let baseline_manifest_path = run_dir().join("baseline/manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&baseline_manifest_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", baseline_manifest_path.display())),
    )
    .expect("parse baseline manifest.json");
    let selected_count = manifest["selected_count"]
        .as_u64()
        .expect("manifest selected_count");

    assert_eq!(
        selected_count, EXPECTED_SUBSET_SIZE,
        "baseline manifest selected_count {selected_count} != expected {EXPECTED_SUBSET_SIZE}"
    );

    let predictions_path = run_dir().join("phase1/predictions.jsonl");
    let pred_lines = std::fs::read_to_string(&predictions_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", predictions_path.display()))
        .lines()
        .count() as u64;

    assert_eq!(
        pred_lines, selected_count,
        "phase1 predictions.jsonl line count {pred_lines} != manifest selected_count {selected_count}"
    );
}

#[test]
#[ignore = "asserts recorded held-out flask metrics.json verbatim; \
            §8 #5 fails honestly on this round — run with --ignored"]
fn rule_set_version_v1_2_on_flask_heldout() {
    // run_meta.json carries the rule_set_version as a top-level field.
    let run_meta_path = run_dir().join("phase1/run_meta.json");
    let run_meta: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&run_meta_path)
            .unwrap_or_else(|e| panic!("read {}: {e}", run_meta_path.display())),
    )
    .expect("parse phase1/run_meta.json");
    let rsv = run_meta["rule_set_version"]
        .as_str()
        .expect("run_meta.json rule_set_version");
    assert_eq!(
        rsv, EXPECTED_RULE_SET_VERSION,
        "phase1/run_meta.json rule_set_version {rsv} != {EXPECTED_RULE_SET_VERSION}"
    );

    // Every prediction's request_id must embed the same rule_set_version
    // in the documented `phase1:<version>:<sha>:<idx>` shape.
    let predictions_path = run_dir().join("phase1/predictions.jsonl");
    let preds = std::fs::read_to_string(&predictions_path)
        .unwrap_or_else(|e| panic!("read {}: {e}", predictions_path.display()));
    let expected_prefix = format!("phase1:{EXPECTED_RULE_SET_VERSION}:");
    for (i, line) in preds.lines().enumerate() {
        let row: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("parse pred row {i}: {e}"));
        let req_id = row["request_id"]
            .as_str()
            .unwrap_or_else(|| panic!("row {i} missing request_id"));
        assert!(
            req_id.starts_with(&expected_prefix),
            "row {i} request_id {req_id} does not start with {expected_prefix}"
        );
    }
}
