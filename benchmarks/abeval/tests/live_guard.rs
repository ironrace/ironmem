use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use abeval::arms::Arm;
use abeval::client::{ArmExecutor, ArmOutcome};
use abeval::corpus::Task;
use abeval::runner::{approval_present, approval_present_with_file, run_task, RunArgs};

static ENV_LOCK: Mutex<()> = Mutex::new(());

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
    let _guard = ENV_LOCK.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Ensure no approval in environment for this assertion.
    std::env::remove_var(abeval::constants::APPROVAL_ENV);

    let err = run_task(RunArgs {
        task: task(),
        arms: vec![Arm::Ironmem],
        dry_run: false,
        execute_live: true,
        budget_usd: Some(1.0),
        approval_file: None,
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
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::set_var(abeval::constants::APPROVAL_ENV, "yes");
    assert!(approval_present());
    for denied in ["false", "0", "no", ""] {
        std::env::set_var(abeval::constants::APPROVAL_ENV, denied);
        assert!(
            !approval_present(),
            "env value {denied:?} must not count as approval"
        );
    }
    std::env::remove_var(abeval::constants::APPROVAL_ENV);
    assert!(!approval_present());
}

#[test]
fn approval_helper_reads_file_sentinel() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var(abeval::constants::APPROVAL_ENV);
    let dir = tempfile::tempdir().unwrap();
    let approval_file = dir.path().join("approval.txt");
    std::fs::write(
        &approval_file,
        format!("{}\n", abeval::constants::APPROVAL_FILE_SENTINEL),
    )
    .unwrap();

    assert!(approval_present_with_file(Some(&approval_file)).unwrap());
}

// NOTE (issue #122): the former `…remains_inert` test asserted that even an
// APPROVED live run stopped at an inert "not enabled in this PR" boundary. That
// contract is intentionally removed here — #122 implements the live executor, so
// an approved run now actually spawns. We do NOT drive the approved+real path in
// tests (it would spawn `claude`); the approved orchestration is covered with
// fakes in `tests/live_executor.rs` (`execute_approved_live_writes_live_metrics_file`),
// the real subprocess plumbing with `printf`/`true`/`false`, and the
// approval-REQUIRED invariant below + `…_spawns_nothing` above.

/// The approval guard is the load-bearing safety invariant: without approval, a
/// live `run` must error before constructing or spawning anything, even when
/// `--execute-live` is set.
#[test]
fn live_run_without_approval_never_spawns() {
    let _guard = ENV_LOCK.lock().unwrap();
    std::env::remove_var(abeval::constants::APPROVAL_ENV);
    let dir = tempfile::tempdir().unwrap();

    let err = run_task(RunArgs {
        task: task(),
        arms: vec![Arm::Ironmem],
        dry_run: false,
        execute_live: true,
        budget_usd: Some(1.0),
        approval_file: None,
        out_dir: dir.path().to_path_buf(),
    })
    .unwrap_err();

    assert!(
        err.to_string().contains("approval"),
        "missing approval must be cited, got: {err}"
    );
    assert!(
        !dir.path().join("abeval-01-x").exists(),
        "no workspace or metrics written when approval is absent"
    );
}
