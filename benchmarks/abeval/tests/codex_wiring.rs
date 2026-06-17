use abeval::client::{ArmOutcome, Usage};
use abeval::report::build_arm_metric;

#[test]
fn build_arm_metric_folds_codex_tokens_and_rework() {
    let outcome = ArmOutcome {
        arm: abeval::arms::Arm::Ironmem,
        usage: Usage { input_tokens: 1000, output_tokens: 200, ..Default::default() },
        codex_usage: Usage {
            input_tokens: 60,            // uncached input
            cache_read_input_tokens: 40, // cached
            output_tokens: 10,
            cache_creation_input_tokens: 0,
        },
        review_rounds: 2,
        fix_commits: 3,
        outcome: "completed".to_string(),
        transcript: String::new(),
    };
    let m = build_arm_metric("task1", "ironmem", &outcome, true);
    // claude 1200 + codex (60+40+10) = 1310.
    assert_eq!(m.tokens_to_done(), 1310);
    assert_eq!(m.codex_input_tokens, 60);
    assert_eq!(m.codex_cache_read_input_tokens, 40);
    assert_eq!(m.codex_output_tokens, 10);
    assert_eq!(m.review_rounds, 2);
    assert_eq!(m.fix_commits, 3);
    assert_eq!(m.rework_loops(), 5);
    assert!(m.is_done()); // completed + green
}

#[test]
fn superpowers_arm_has_zero_codex_and_rework() {
    let outcome = ArmOutcome {
        arm: abeval::arms::Arm::Superpowers,
        usage: Usage { input_tokens: 500, output_tokens: 100, ..Default::default() },
        codex_usage: Usage::default(),
        review_rounds: 0,
        fix_commits: 0,
        outcome: "completed".to_string(),
        transcript: String::new(),
    };
    let m = build_arm_metric("task1", "superpowers", &outcome, true);
    assert_eq!(m.tokens_to_done(), 600);
    assert_eq!(m.rework_loops(), 0);
}
