use abeval::report::{render_report, MetricsInput, TaskMetric};

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
