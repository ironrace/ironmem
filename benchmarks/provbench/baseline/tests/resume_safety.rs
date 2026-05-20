//! End-to-end runner test exercising the full atomic + resume path
//! **without network and without fixtures**.
//!
//! Uses `--dry-run` mode (fabricates a "valid" decision per row, zero
//! usage → zero cost) against the existing Task 5 fixtures. Verifies:
//!   1. A clean run scores every batch and writes `predictions.jsonl`.
//!   2. A second run with `--resume` skips already-done rows and leaves
//!      `predictions.jsonl` byte-identical.
//!   3. Resume with a tampered manifest `content_hash` returns `Err`.

use provbench_baseline::manifest::SampleManifest;
use provbench_baseline::runner::{run, RunnerOpts};
use provbench_baseline::sample::PerStratumTargets;
use std::path::Path;

#[tokio::test]
async fn resume_skips_already_scored_rows_and_verifies_hash() {
    // 1. Build a tiny manifest from Task 5's fixtures.
    let manifest = SampleManifest::from_corpus(
        Path::new("fixtures/sample_corpus.jsonl"),
        Path::new("fixtures/sample_facts.jsonl"),
        Path::new("fixtures/sample_diffs"),
        0xC0DE_BABE_DEAD_BEEFu64,
        PerStratumTargets::default(),
        25.0,
    )
    .expect("manifest builds");

    // 2. Persist to a tempdir run_dir.
    let tmp = tempfile::tempdir().unwrap();
    let run_dir = tmp.path().to_path_buf();
    manifest
        .save_atomic(&run_dir.join("manifest.json"))
        .expect("save manifest");

    // 3. First run in dry-run mode → all batches scored.
    let first = run(RunnerOpts {
        run_dir: run_dir.clone(),
        manifest: manifest.clone(),
        budget_usd: 25.0,
        resume: false,
        dry_run: true,
        fixture_mode: None,
        max_batches: None,
        max_concurrency: 1,
        max_input_tokens_per_minute: 0,
        client_override: None,
    })
    .await
    .expect("first run");
    assert!(!first.aborted, "first run must not abort");
    assert_eq!(
        first.batches_completed, first.batches_total,
        "first run must score every batch"
    );
    assert!(
        first.batches_total > 0,
        "fixture must produce at least one batch"
    );

    let predictions_first =
        std::fs::read_to_string(run_dir.join("predictions.jsonl")).expect("read predictions");
    let rows_first = predictions_first.lines().count();
    assert!(rows_first > 0, "first run must write at least one row");
    assert_eq!(
        rows_first, first.rows_scored,
        "predictions.jsonl line count must match rows_scored"
    );

    // 4. Second run with --resume → everything already done.
    let second = run(RunnerOpts {
        run_dir: run_dir.clone(),
        manifest: manifest.clone(),
        budget_usd: 25.0,
        resume: true,
        dry_run: true,
        fixture_mode: None,
        max_batches: None,
        max_concurrency: 1,
        max_input_tokens_per_minute: 0,
        client_override: None,
    })
    .await
    .expect("second run");
    assert!(!second.aborted, "second run must not abort");
    assert_eq!(
        second.batches_completed, 0,
        "second run must skip every batch (all rows already scored)"
    );
    assert_eq!(
        second.batches_skipped_resume, first.batches_total,
        "every batch must be marked skipped_resume"
    );
    let predictions_second =
        std::fs::read_to_string(run_dir.join("predictions.jsonl")).expect("read predictions");
    assert_eq!(
        predictions_first, predictions_second,
        "resume must not rewrite predictions.jsonl"
    );

    // 5. Tampered manifest → resume must reject.
    let mut tampered = manifest.clone();
    tampered.content_hash = "deadbeef".repeat(8);
    let result = run(RunnerOpts {
        run_dir: run_dir.clone(),
        manifest: tampered,
        budget_usd: 25.0,
        resume: true,
        dry_run: true,
        fixture_mode: None,
        max_batches: None,
        max_concurrency: 1,
        max_input_tokens_per_minute: 0,
        client_override: None,
    })
    .await;
    assert!(result.is_err(), "resume must reject hash mismatch");
}
