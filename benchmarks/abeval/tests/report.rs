use abeval::report::{load_metrics, metrics_from_run_dir, render_report, MetricsInput, TaskMetric};

fn fixture(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}

fn merged(arm: &str, tokens: u64) -> TaskMetric {
    TaskMetric {
        arm: arm.to_string(),
        task_key: format!("{arm}-{tokens}"),
        outcome: "merged".to_string(),
        ci_green: true,
        estimated: false,
        input_tokens: tokens as u32,
        output_tokens: 0,
        cache_creation_input_tokens: 0,
        cache_read_input_tokens: 0,
        review_rounds: 1,
        fix_commits: 1,
    }
}

#[test]
fn tokens_to_done_sums_four_components() {
    let m = TaskMetric {
        input_tokens: 10,
        output_tokens: 5,
        cache_creation_input_tokens: 3,
        cache_read_input_tokens: 2,
        ..merged("ironmem", 0)
    };
    assert_eq!(m.tokens_to_done(), 20);
}

#[test]
fn rework_loops_is_review_plus_fix() {
    let m = merged("ironmem", 100);
    assert_eq!(m.rework_loops(), 2); // review_rounds 1 + fix_commits 1
}

#[test]
fn smoke_run_withholds_headline() {
    let input = MetricsInput {
        evidence_class: "smoke".to_string(),
        tasks: vec![],
    };
    let out = render_report(&input);
    assert!(out.contains("SMOKE"));
    assert!(out.contains("non-headline"));
    assert!(!out.contains("DELTA"));
}

#[test]
fn under_eight_live_withholds_delta() {
    let mut tasks = Vec::new();
    for i in 0..5 {
        tasks.push(merged("ironmem", 100 + i));
        tasks.push(merged("superpowers", 200 + i));
    }
    let input = MetricsInput {
        evidence_class: "live".to_string(),
        tasks,
    };
    let out = render_report(&input);
    assert!(!out.contains("DELTA"), "below n=8 must withhold deltas");
}

#[test]
fn duplicate_task_keys_do_not_satisfy_headline_gate() {
    let mut tasks = Vec::new();
    for _ in 0..8 {
        tasks.push(merged("ironmem", 100));
        tasks.push(merged("superpowers", 200));
    }
    let input = MetricsInput {
        evidence_class: "live".to_string(),
        tasks,
    };
    let out = render_report(&input);
    assert!(
        !out.contains("DELTA"),
        "duplicate task_key rows must not inflate n=8"
    );
    assert!(out.contains("duplicates_ignored=7"));
}

#[test]
fn estimated_rows_do_not_satisfy_headline_gate() {
    let mut tasks = Vec::new();
    for i in 0..8 {
        let mut iron = merged("ironmem", 100 + i);
        iron.estimated = true;
        tasks.push(iron);

        let mut superpowers = merged("superpowers", 200 + i);
        superpowers.estimated = true;
        tasks.push(superpowers);
    }
    let input = MetricsInput {
        evidence_class: "live".to_string(),
        tasks,
    };
    let out = render_report(&input);
    assert!(
        !out.contains("DELTA"),
        "estimated rows must never enter headline deltas"
    );
}

#[test]
fn headline_gate_requires_both_fixed_arms() {
    let mut tasks = Vec::new();
    for i in 0..8 {
        tasks.push(merged("ironmem", 100 + i));
    }
    let input = MetricsInput {
        evidence_class: "live".to_string(),
        tasks,
    };
    let out = render_report(&input);
    assert!(
        !out.contains("DELTA"),
        "one-arm metrics cannot produce a cross-arm delta"
    );
    assert!(out.contains("both ironmem and superpowers"));
}

#[test]
fn failed_and_smoke_attempts_do_not_count_toward_gate() {
    let mut tasks = Vec::new();
    // 8 merged+green per arm, plus failed/non-green that must NOT count.
    for i in 0..8 {
        tasks.push(merged("ironmem", 100 + i));
        tasks.push(merged("superpowers", 200 + i));
    }
    // Failed attempts (visible, non-counting).
    let mut failed = merged("ironmem", 999);
    failed.outcome = "failed".to_string();
    failed.ci_green = false;
    tasks.push(failed);
    let input = MetricsInput {
        evidence_class: "live".to_string(),
        tasks,
    };
    let out = render_report(&input);
    assert!(
        out.contains("DELTA"),
        "8 merged+green per arm passes the gate"
    );
    assert!(
        out.contains("n=8"),
        "delta must be confidence-qualified with n"
    );
}

#[test]
fn live_8_per_arm_fixture_yields_exact_delta_math() {
    // Numeric coverage of the DELTA payload (means/spreads/rework/merged-rate),
    // loaded from the committed fixture via the --metrics path (load_metrics).
    let input = load_metrics(fixture("live_8_per_arm.json")).expect("fixture loads");
    let out = render_report(&input);
    // Per-arm visible numbers.
    assert!(
        out.contains("arm ironmem: attempted=8 attempted_tokens=1620 merged=8 completed=8 \
                      mean_tokens=202.5 mean_rework=1.8"),
        "ironmem per-arm line wrong:\n{out}"
    );
    assert!(
        out.contains("arm superpowers: attempted=8 attempted_tokens=2820 merged=8 completed=8 \
                      mean_tokens=352.5 mean_rework=2.4"),
        "superpowers per-arm line wrong:\n{out}"
    );
    // Headline delta with exact means/spreads/rework/merged-rate.
    assert!(out.contains("DELTA (n=8, confidence-qualified):"), "missing delta:\n{out}");
    assert!(
        out.contains("tokens-to-done: ironmem mean=202.5 (spread 105), \
                      superpowers mean=352.5 (spread 105)"),
        "tokens-to-done delta wrong:\n{out}"
    );
    assert!(
        out.contains("rework_loops: ironmem mean=1.8, superpowers mean=2.4"),
        "rework delta wrong:\n{out}"
    );
    assert!(
        out.contains("merged-rate: ironmem 8/8, superpowers 8/8"),
        "merged-rate wrong:\n{out}"
    );
}

#[test]
fn live_under_8_fixture_withholds_delta() {
    let input = load_metrics(fixture("live_under_8.json")).expect("fixture loads");
    let out = render_report(&input);
    assert!(!out.contains("DELTA"), "under-8 fixture must withhold deltas:\n{out}");
    assert!(out.contains("both ironmem and superpowers"));
}

#[test]
fn asymmetric_completed_counts_use_min_for_n() {
    // 10 merged+green ironmem vs 8 superpowers → gate passes, n = min = 8.
    let mut tasks = Vec::new();
    for i in 0..10 {
        tasks.push(merged("ironmem", 100 + i));
    }
    for i in 0..8 {
        tasks.push(merged("superpowers", 200 + i));
    }
    let input = MetricsInput {
        evidence_class: "live".to_string(),
        tasks,
    };
    let out = render_report(&input);
    assert!(out.contains("DELTA (n=8"), "n must be min(10,8)=8, not 10:\n{out}");
    assert!(out.contains("merged-rate: ironmem 10/10, superpowers 8/8"));
}

#[test]
fn metrics_from_run_dir_rejects_live_run_directory() {
    // This PR ships no paid runs; a "live" run dir cannot legitimately exist and
    // must be a hard error, not a silent smoke downgrade with fabricated rows.
    let dir = tempfile::tempdir().unwrap();
    let task = dir.path().join("t1");
    std::fs::create_dir_all(&task).unwrap();
    std::fs::write(
        task.join("run_meta.json"),
        r#"{"evidence_class":"live","per_arm":[]}"#,
    )
    .unwrap();
    let err = metrics_from_run_dir(dir.path()).unwrap_err();
    assert!(
        err.to_string().contains("live run directories are not supported")
            || format!("{err:#}").contains("live run directories are not supported"),
        "expected live-dir rejection, got: {err:#}"
    );
}

#[test]
fn load_metrics_rejects_unknown_evidence_class() {
    // load_metrics is the live-evidence ingestion path; an unrecognized
    // evidence_class (typo/wrong-case) must error, not silently render as SMOKE
    // and withhold real deltas. Keeps it symmetric with metrics_from_run_dir.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("m.json");
    std::fs::write(&path, r#"{"evidence_class":"Live","tasks":[]}"#).unwrap();
    let err = load_metrics(&path).unwrap_err();
    assert!(
        format!("{err:#}").contains("invalid evidence_class"),
        "expected evidence_class rejection, got: {err:#}"
    );
}

#[test]
fn metrics_from_run_dir_rejects_missing_evidence_class() {
    // A run_meta.json with no evidence_class key (→ None) must hard-bail, not
    // default to smoke — the typed parse + `other =>` arm guards this.
    let dir = tempfile::tempdir().unwrap();
    let task = dir.path().join("t1");
    std::fs::create_dir_all(&task).unwrap();
    std::fs::write(task.join("run_meta.json"), r#"{"per_arm":[]}"#).unwrap();
    let err = metrics_from_run_dir(dir.path()).unwrap_err();
    assert!(
        format!("{err:#}").contains("invalid or missing evidence_class"),
        "expected missing-evidence_class rejection, got: {err:#}"
    );
}

#[test]
fn metrics_from_run_dir_skips_arm_without_usage_json() {
    // per_arm lists two arms but only one has usage.json on disk; the arm with
    // no usage.json is skipped (only the present arm appears).
    let dir = tempfile::tempdir().unwrap();
    let task = dir.path().join("t1");
    let iron = task.join("ironmem");
    std::fs::create_dir_all(&iron).unwrap();
    std::fs::write(
        task.join("run_meta.json"),
        r#"{"evidence_class":"smoke","per_arm":[{"arm":"ironmem","outcome":"completed","usage":{}},{"arm":"superpowers","outcome":"completed","usage":{}}]}"#,
    )
    .unwrap();
    std::fs::write(
        iron.join("usage.json"),
        r#"{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}"#,
    )
    .unwrap();
    let input = metrics_from_run_dir(dir.path()).unwrap();
    assert_eq!(input.tasks.len(), 1, "only the arm with usage.json should appear");
    assert_eq!(input.tasks[0].arm, "ironmem");
}

#[test]
fn metrics_from_run_dir_reads_real_outcome_not_hardcoded() {
    // Prove the per-arm outcome is read from run_meta.json, not hardcoded.
    let dir = tempfile::tempdir().unwrap();
    let task = dir.path().join("t1");
    let arm = task.join("ironmem");
    std::fs::create_dir_all(&arm).unwrap();
    std::fs::write(
        task.join("run_meta.json"),
        r#"{"evidence_class":"smoke","per_arm":[{"arm":"ironmem","outcome":"failed","usage":{}}]}"#,
    )
    .unwrap();
    std::fs::write(
        arm.join("usage.json"),
        r#"{"input_tokens":1,"output_tokens":2,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}"#,
    )
    .unwrap();
    let input = metrics_from_run_dir(dir.path()).unwrap();
    assert_eq!(input.evidence_class, "smoke");
    assert_eq!(input.tasks.len(), 1);
    assert_eq!(input.tasks[0].outcome, "failed", "outcome must reflect run_meta, not a hardcoded 'completed'");
}

#[test]
fn delta_requires_live_evidence_even_with_enough_tasks() {
    let mut tasks = Vec::new();
    for i in 0..8 {
        tasks.push(merged("ironmem", 100 + i));
        tasks.push(merged("superpowers", 200 + i));
    }
    let input = MetricsInput {
        evidence_class: "smoke".to_string(),
        tasks,
    };
    let out = render_report(&input);
    assert!(
        !out.contains("DELTA"),
        "smoke evidence never yields a headline"
    );
}
