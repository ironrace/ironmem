use abeval::arms::Arm;
use abeval::client::{ArmExecutor, DryRunExecutor};
use abeval::corpus::Task;
use abeval::runner::{run_task, RunArgs};

fn task() -> Task {
    Task {
        id: "abeval-01-x".to_string(),
        title: "t".to_string(),
        source: "issue:#95".to_string(),
        repo_scope: vec!["crates/**".to_string()],
        prompt: "do the thing".to_string(),
        acceptance: vec!["ok".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
        base_commit: "ce2b27f2bcf3d318e0142ff5a1ece578559d9261".to_string(),
    }
}

#[test]
fn dry_run_usage_is_deterministic() {
    let exec = DryRunExecutor;
    let a = exec.execute(&task(), Arm::Ironmem).unwrap();
    let b = exec.execute(&task(), Arm::Ironmem).unwrap();
    assert_eq!(a.usage.input_tokens, b.usage.input_tokens);
    assert_eq!(a.usage.output_tokens, b.usage.output_tokens);
}

#[test]
fn dry_run_writes_both_arm_artifacts_no_network() {
    let dir = tempfile::tempdir().unwrap();
    let summary = run_task(RunArgs {
        task: task(),
        arms: vec![Arm::Ironmem, Arm::Superpowers],
        dry_run: true,
        execute_live: false,
        budget_usd: None,
        approval_file: None,
        out_dir: dir.path().to_path_buf(),
    })
    .unwrap();

    assert_eq!(summary.arms_run, 2);

    let meta = dir.path().join("abeval-01-x").join("run_meta.json");
    let body = std::fs::read_to_string(&meta).unwrap();
    let json: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["approved_paid_run"], false);
    assert_eq!(json["evidence_class"], "smoke");

    for arm in ["ironmem", "superpowers"] {
        let usage = dir.path().join("abeval-01-x").join(arm).join("usage.json");
        assert!(usage.exists(), "missing usage.json for {arm}");
    }
}

#[test]
fn run_task_rejects_unsafe_task_id_before_writing() {
    let dir = tempfile::tempdir().unwrap();
    let mut bad = task();
    bad.id = "../escape".to_string();
    let err = run_task(RunArgs {
        task: bad,
        arms: vec![Arm::Ironmem],
        dry_run: true,
        execute_live: false,
        budget_usd: None,
        approval_file: None,
        out_dir: dir.path().to_path_buf(),
    })
    .unwrap_err();
    assert!(
        err.to_string().contains("unsafe task id"),
        "expected unsafe-id rejection, got: {err}"
    );
    // Nothing escaped the out_dir.
    assert!(!dir.path().parent().unwrap().join("escape").exists());
}
