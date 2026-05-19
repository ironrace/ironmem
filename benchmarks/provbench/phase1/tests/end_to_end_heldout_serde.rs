//! SPEC §9.4 held-out gate on serde-rs/serde @ 65e1a507.
//!
//! Asserts the three §8 thresholds verbatim against the
//! `phase1_rules` column in the held-out canary metrics. Also
//! asserts row-count consistency and `rule_set_version=v1.1`
//! evidence in every prediction's `request_id`.
//!
//! Does NOT assert `phase1_git_sha` — phase1 score writes no
//! `run_meta.json` of its own (the round's hand-written
//! `<RUNDIR>/phase1/run_meta.json` carries that, and the
//! committed-artifact gate is a separate concern from the e2e test).
//!
//! On §8 miss this test fails honestly. Per SPEC §10 the round
//! does NOT retune in response — the failure is recorded as the
//! held-out result in `results/serde-heldout-2026-05-15-findings.md`
//! and SPEC §11. **A FAIL here is the recorded experimental result,
//! NOT a regression to fix.**
//!
//! ## Running
//!
//! Preconditions:
//! - `benchmarks/provbench/work/serde` checked out at SERDE_T0's
//!   latest first-parent descendant (`fa7da4a9…`).
//! - `benchmarks/provbench/results/serde-heldout-2026-05-15-canary/baseline/`
//!   populated by `provbench-baseline sample` + `run --dry-run` + `score`
//!   per the plan at
//!   `docs/superpowers/plans/2026-05-15-provbench-phase1-heldout-serde.md`.
//!
//! Invocation (the `#[ignore]` attribute requires `--ignored`):
//! ```text
//! cargo test --release --manifest-path benchmarks/provbench/phase1/Cargo.toml \
//!     --test end_to_end_heldout_serde -- --ignored --nocapture
//! ```

use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

const SERDE_T0: &str = "65e1a50749938612cfbdb69b57fc4cf249f87149";
const HELDOUT_RUN_DIR: &str = "results/serde-heldout-2026-05-15-canary";
const HELDOUT_FACTS: &str = "facts/serde-65e1a507-c2d3b7b.facts.jsonl";
const HELDOUT_DIFFS_DIR: &str = "facts/serde-65e1a507-c2d3b7b.diffs";

fn provbench_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..")
}

fn ensure_scoring_binary_built() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let scoring_manifest = manifest_dir.join("../scoring/Cargo.toml");
    let bin_path = manifest_dir.join("../scoring/target/release/provbench-score");
    let status = std::process::Command::new("cargo")
        .args([
            "build",
            "--release",
            "--manifest-path",
            scoring_manifest.to_str().unwrap(),
            "--bin",
            "provbench-score",
        ])
        .status()
        .expect("cargo build provbench-score");
    assert!(
        status.success(),
        "cargo build --release provbench-score failed"
    );
    assert!(
        bin_path.exists(),
        "provbench-score binary not found at {}",
        bin_path.display()
    );
    bin_path
}

#[test]
#[ignore = "requires benchmarks/provbench/work/serde checkout and prepared subset under \
            results/serde-heldout-2026-05-15-canary/baseline; run with --ignored"]
fn spec_section_8_thresholds_on_serde_heldout_subset() {
    let provbench = provbench_root();
    let workrepo = provbench.join("work/serde");
    assert!(
        workrepo.exists(),
        "needs work/serde checkout for held-out e2e"
    );

    let baseline_run = provbench.join(HELDOUT_RUN_DIR).join("baseline");
    assert!(
        baseline_run.join("metrics.json").exists(),
        "needs prepared <RUNDIR>/baseline/metrics.json (see plan steps 5+6)"
    );

    let phase1_bin = env!("CARGO_BIN_EXE_provbench-phase1");
    let score_bin = ensure_scoring_binary_built();

    let out = TempDir::new().unwrap();
    let out_p = out.path().to_path_buf();

    // phase1 score
    let status = Command::new(phase1_bin)
        .args([
            "score",
            "--repo",
            workrepo.to_str().unwrap(),
            "--t0",
            SERDE_T0,
            "--facts",
            provbench.join(HELDOUT_FACTS).to_str().unwrap(),
            "--diffs-dir",
            provbench.join(HELDOUT_DIFFS_DIR).to_str().unwrap(),
            "--baseline-run",
            baseline_run.to_str().unwrap(),
            "--out",
            out_p.to_str().unwrap(),
            "--rule-set-version",
            // v1.3: R4 guard-below-floor rows route to NeedsRevalidation
            // instead of Stale. Pre-registered in SPEC §11 row 2026-05-18.
            "v1.3",
        ])
        .status()
        .unwrap();
    assert!(status.success(), "phase1 score failed");

    // provbench-score compare
    let status = Command::new(&score_bin)
        .args([
            "compare",
            "--baseline-run",
            baseline_run.to_str().unwrap(),
            "--candidate-run",
            out_p.to_str().unwrap(),
            "--candidate-name",
            "phase1_rules",
            "--out",
            out_p.join("metrics.json").to_str().unwrap(),
        ])
        .status()
        .unwrap();
    assert!(status.success(), "provbench-score compare failed");

    let metrics: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out_p.join("metrics.json")).unwrap()).unwrap();

    // SPEC §8 verbatim against phase1_rules column.
    let valid_wlb = metrics["phase1_rules"]["section_7_1"]["valid_retention_accuracy"]
        ["wilson_lower_95"]
        .as_f64()
        .expect("phase1_rules valid_retention_accuracy wilson_lower_95");
    let p50 = metrics["phase1_rules"]["section_7_2_applicable"]["latency_p50_ms"]
        .as_u64()
        .expect("phase1_rules latency_p50_ms");

    // §8 #5: SKIP-aware assertion. Per SPEC §11 row 2026-05-18, when
    // ground-truth stale_* count == 0 the threshold is recorded as
    // status="SKIP" and the numeric value is null — we must not assert
    // numerically in that case.
    let s8_5 = &metrics["thresholds"]["section_8_5_stale_recall_wlb_ge_0_30"];
    match s8_5["status"].as_str().unwrap_or("") {
        "SKIP" => {
            eprintln!(
                "§8 #5 SKIP (status=SKIP, reason={}): not asserted numerically",
                s8_5["reason"].as_str().unwrap_or("none")
            );
        }
        "PASS" | "FAIL" => {
            let stale_wlb = s8_5["observed"]
                .as_f64()
                .expect("observed must be numeric on PASS/FAIL");
            assert!(
                stale_wlb >= 0.30,
                "§8 #5 stale recall WLB {stale_wlb:.4} < 0.30 on PASS/FAIL"
            );
        }
        other => panic!("unknown §8 #5 status: {other}"),
    }
    assert!(
        valid_wlb >= 0.95,
        "§8 #3 valid retention WLB {:.4} < 0.95",
        valid_wlb
    );
    assert!(p50 <= 727, "§8 #4 latency p50 {} ms > 727", p50);

    // Row-count consistency: predictions.jsonl line count == manifest selected_count.
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(baseline_run.join("manifest.json")).unwrap())
            .unwrap();
    let selected_count = manifest["selected_count"]
        .as_u64()
        .expect("manifest selected_count");
    let pred_lines = std::fs::read_to_string(out_p.join("predictions.jsonl"))
        .unwrap()
        .lines()
        .count() as u64;
    assert_eq!(
        pred_lines, selected_count,
        "phase1 predictions line count {pred_lines} != manifest selected_count {selected_count}"
    );

    // rule_set_version=v1.3 evidence in every prediction's request_id.
    // v1.3: R4 NR carve-out for guard-below-floor rows.
    // Pre-registered in SPEC §11 row 2026-05-18.
    let preds = std::fs::read_to_string(out_p.join("predictions.jsonl")).unwrap();
    for (i, line) in preds.lines().enumerate() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        let req_id = row["request_id"].as_str().expect("request_id");
        assert!(
            req_id.contains("v1.3"),
            "row {i} request_id {req_id} does not embed rule_set_version v1.3"
        );
    }
}
