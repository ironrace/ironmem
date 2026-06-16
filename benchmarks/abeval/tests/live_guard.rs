use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use abeval::arms::Arm;
use abeval::client::{ArmExecutor, ArmOutcome};
use abeval::corpus::Task;
use abeval::runner::{approval_present, run_task, RunArgs};

fn task() -> Task {
    Task {
        id: "abeval-01-x".to_string(),
        title: "t".to_string(),
        source: "issue:#95".to_string(),
        repo_scope: vec!["crates/**".to_string()],
        prompt: "p".to_string(),
        acceptance: vec!["ok".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
    }
}

/// Spy executor that counts how many times execution was attempted.
struct SpyExecutor {
    spawned: Arc<AtomicUsize>,
}

impl ArmExecutor for SpyExecutor {
    fn execute(&self, _task: &Task, _arm: Arm) -> anyhow::Result<ArmOutcome> {
        self.spawned.fetch_add(1, Ordering::SeqCst);
        anyhow::bail!("spy should never run in tests")
    }
}

#[test]
fn execute_live_without_approval_errors_and_spawns_nothing() {
    let dir = tempfile::tempdir().unwrap();
    // Ensure no approval in environment for this assertion.
    std::env::remove_var(abeval::constants::APPROVAL_ENV);

    let err = run_task(RunArgs {
        task: task(),
        arms: vec![Arm::Ironmem],
        dry_run: false,
        execute_live: true,
        budget_usd: Some(1.0),
        out_dir: dir.path().to_path_buf(),
    })
    .unwrap_err();

    assert!(
        err.to_string().contains("approval"),
        "error should cite missing approval, got: {err}"
    );
    // No artifacts written → guard fired before any execution.
    assert!(!dir.path().join("abeval-01-x").join("ironmem").exists());
}

#[test]
fn guard_blocks_before_executor_runs() {
    let spawned = Arc::new(AtomicUsize::new(0));
    let exec = SpyExecutor {
        spawned: spawned.clone(),
    };
    // The guard is callable directly and must refuse without approval
    // BEFORE touching the executor.
    let blocked = abeval::runner::guard_live_then_run(
        &task(),
        &[Arm::Ironmem],
        /* approved = */ false,
        &exec,
        std::path::Path::new("/nonexistent"),
    );
    assert!(blocked.is_err());
    assert_eq!(
        spawned.load(Ordering::SeqCst),
        0,
        "executor must not run when blocked"
    );
}

#[test]
fn approval_helper_reads_env() {
    std::env::set_var(abeval::constants::APPROVAL_ENV, "yes");
    assert!(approval_present());
    std::env::remove_var(abeval::constants::APPROVAL_ENV);
    assert!(!approval_present());
}
