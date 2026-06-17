//! Tests for the WorkspaceProvisioner seam, base resolution, and LiveExecutor
//! with provision ordering (Task 4).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use abeval::arms::Arm;
use abeval::client::{
    resolve_base_commit, ArmExecutor, ArmOutcome, CommandOutput, CommandRunner, IronmemArmRunner,
    LiveExecutor, ProvisionRequest, Usage, WorkspaceProvisioner,
};
use abeval::corpus::{BaseCommit, Task};

fn task(base: &str) -> Task {
    let base_commit = if base.is_empty() {
        BaseCommit::unset()
    } else {
        BaseCommit::parse(base).unwrap()
    };
    Task {
        id: "t1".to_string(),
        title: "T".to_string(),
        source: "issue:#1".to_string(),
        repo_scope: vec!["crates/ironmem/src/lib.rs".to_string()],
        prompt: "p".to_string(),
        acceptance: vec!["a".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
        base_commit,
    }
}

#[test]
fn base_resolution_task_pin_used_when_no_override() {
    let t = task("abcdef1234567890abcdef1234567890abcdef12");
    let resolved = resolve_base_commit(&t, None).unwrap();
    assert_eq!(
        resolved.as_str(),
        "abcdef1234567890abcdef1234567890abcdef12"
    );
}

#[test]
fn base_resolution_pin_plus_override_is_illegal() {
    // FIX 1: resolve_base_commit is the single authority — overriding a pin from
    // the CLI is rejected, not silently won by the pin.
    let t = task("abcdef1234567890abcdef1234567890abcdef12");
    let err = resolve_base_commit(&t, Some("0000000"))
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("cannot override pinned base_commit"),
        "got: {err}"
    );
    assert!(err.contains("t1"), "got: {err}");
}

#[test]
fn base_resolution_run_override_used_when_task_empty() {
    let t = task("");
    let resolved = resolve_base_commit(&t, Some("0123456")).unwrap();
    assert_eq!(resolved.as_str(), "0123456");
}

#[test]
fn base_resolution_both_absent_fails_loud() {
    let t = task("");
    let err = resolve_base_commit(&t, None).unwrap_err().to_string();
    assert!(err.contains("refusing to provision"), "got: {err}");
    assert!(err.contains("t1"), "got: {err}");
}

#[test]
fn task_pin_cannot_be_invalid_in_memory() {
    // FIX 4: the `BaseCommit` newtype makes a non-empty INVALID task pin
    // unrepresentable — the only entry points are `parse` (validates) and serde
    // `try_from` (validates). An invalid pin string is therefore rejected before
    // a `Task` can exist, so `resolve_base_commit` never sees one. We assert the
    // boundary directly: an invalid pin string fails to construct a BaseCommit.
    let err = BaseCommit::parse("zzz-not-hex").unwrap_err().to_string();
    assert!(err.contains("invalid base_commit"), "got: {err}");

    let serde_err = serde_json::from_value::<Task>(serde_json::json!({
        "id": "t1",
        "title": "T",
        "source": "issue:#1",
        "repo_scope": ["crates/ironmem/src/lib.rs"],
        "prompt": "p",
        "acceptance": ["a"],
        "gates": ["cargo test"],
        "base_commit": "zzz"
    }))
    .unwrap_err()
    .to_string();
    assert!(
        serde_err.contains("invalid base_commit"),
        "got: {serde_err}"
    );
}

#[test]
fn base_resolution_invalid_run_override_bails_with_override_message() {
    let t = task(""); // no task pin -> override path is taken
    let err = resolve_base_commit(&t, Some("zzz-not-hex"))
        .unwrap_err()
        .to_string();
    assert!(err.contains("--base-sha is"), "got: {err}");
    assert!(err.contains("invalid base_commit"), "got: {err}");
}

// --- Fake provisioner and runner for integration ---

type ProvisionCalls = Arc<Mutex<Vec<(String, PathBuf)>>>;

struct FakeProvisioner {
    calls: ProvisionCalls,
}

impl WorkspaceProvisioner for FakeProvisioner {
    fn provision(&self, req: &ProvisionRequest) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((req.base_commit.to_string(), req.workspace.to_path_buf()));
        Ok(())
    }
}

type RunnerRan = Arc<Mutex<bool>>;

struct FakeRunner {
    stdout: String,
    ran: RunnerRan,
}

impl CommandRunner for FakeRunner {
    fn run(&self, _p: &str, _a: &[String], _w: &Path) -> anyhow::Result<CommandOutput> {
        *self.ran.lock().unwrap() = true;
        Ok(CommandOutput {
            stdout: self.stdout.clone(),
            success: true,
        })
    }
}

// Contract changed (METRICS_SPEC §12 2026-06-17): provision-before-run with a
// resolved base is verified through the single-command arm = superpowers; the
// ironmem arm now delegates to the IronmemArmRunner and never reaches the
// CommandRunner, so this provision/run ordering test drives superpowers.
#[test]
fn execute_provisions_before_running_with_resolved_base() {
    let calls: ProvisionCalls = Arc::new(Mutex::new(Vec::new()));
    let ran: RunnerRan = Arc::new(Mutex::new(false));
    let prov = FakeProvisioner {
        calls: calls.clone(),
    };
    let runner = FakeRunner {
        stdout: r#"{"is_error":false,"result":"ok","usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#.to_string(),
        ran: ran.clone(),
    };
    let exec = LiveExecutor::new(runner, prov, PathBuf::from("/tmp/ws-root"), None);
    let t = task("abcdef1234567890abcdef1234567890abcdef12");
    let out = exec.execute(&t, Arm::Superpowers).unwrap();
    assert_eq!(out.usage.total(), 15);
    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "abcdef1234567890abcdef1234567890abcdef12");
    assert_eq!(
        captured[0].1,
        PathBuf::from("/tmp/ws-root").join("t1").join("superpowers")
    );
    assert!(*ran.lock().unwrap(), "runner.run must be called");
}

#[test]
fn execute_forwards_run_override_when_task_pin_empty() {
    let calls: ProvisionCalls = Arc::new(Mutex::new(Vec::new()));
    let ran: RunnerRan = Arc::new(Mutex::new(false));
    let prov = FakeProvisioner {
        calls: calls.clone(),
    };
    let runner = FakeRunner {
        stdout: r#"{"is_error":false,"result":"ok","usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#.to_string(),
        ran,
    };
    let exec = LiveExecutor::new(
        runner,
        prov,
        PathBuf::from("/tmp/ws-root"),
        Some("0123456".to_string()),
    );
    // Contract changed (METRICS_SPEC §12 2026-06-17): the run-override forwarding
    // is verified through the single-command arm = superpowers; the ironmem arm
    // now delegates to the IronmemArmRunner and never reaches the CommandRunner.
    let out = exec.execute(&task(""), Arm::Superpowers).unwrap();
    assert_eq!(out.usage.total(), 15);
    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "0123456");
}

#[test]
fn execute_short_circuits_when_base_unresolved() {
    let calls: ProvisionCalls = Arc::new(Mutex::new(Vec::new()));
    let ran: RunnerRan = Arc::new(Mutex::new(false));
    let prov = FakeProvisioner {
        calls: calls.clone(),
    };
    let runner = FakeRunner {
        stdout: String::new(),
        ran: ran.clone(),
    };
    let exec = LiveExecutor::new(runner, prov, PathBuf::from("/tmp/ws-root"), None);
    let t = task(""); // no task pin, no run override
    let err = exec.execute(&t, Arm::Ironmem).unwrap_err().to_string();
    assert!(err.contains("refusing to provision"), "got: {err}");
    assert!(!*ran.lock().unwrap(), "runner.run must not be reached");
    assert!(
        calls.lock().unwrap().is_empty(),
        "provision must not be reached"
    );
}

// --- Fix 5: ironmem arm ordering — provision must complete before ironmem_runner.run ---

type OrderLog = Arc<Mutex<Vec<&'static str>>>;

/// Recording provisioner: pushes "provision" into the shared order log.
struct OrderingProvisioner {
    calls: ProvisionCalls,
    order: OrderLog,
}

impl WorkspaceProvisioner for OrderingProvisioner {
    fn provision(&self, req: &ProvisionRequest) -> anyhow::Result<()> {
        self.calls
            .lock()
            .unwrap()
            .push((req.base_commit.to_string(), req.workspace.to_path_buf()));
        self.order.lock().unwrap().push("provision");
        Ok(())
    }
}

/// Fake ironmem runner: asserts "provision" already appears in the order log
/// at the moment `run` is called, then pushes "ironmem_run".
struct OrderingIronmemRunner {
    order: OrderLog,
}

impl IronmemArmRunner for OrderingIronmemRunner {
    fn run(&self, _task: &Task, _workspace: &Path, _out_task_dir: &Path) -> anyhow::Result<ArmOutcome> {
        {
            let log = self.order.lock().unwrap();
            assert!(
                log.contains(&"provision"),
                "ironmem_runner.run was called before provision; order so far: {log:?}"
            );
        }
        self.order.lock().unwrap().push("ironmem_run");
        Ok(ArmOutcome {
            arm: Arm::Ironmem,
            usage: Usage {
                input_tokens: 7,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
                output_tokens: 3,
            },
            codex_usage: Usage::default(),
            review_rounds: 1,
            fix_commits: 2,
            outcome: "completed".to_string(),
            transcript: "fake-ironmem".to_string(),
        })
    }
}

#[test]
fn ironmem_arm_provision_runs_before_ironmem_runner() {
    let order: OrderLog = Arc::new(Mutex::new(Vec::new()));
    let calls: ProvisionCalls = Arc::new(Mutex::new(Vec::new()));

    let prov = OrderingProvisioner {
        calls: calls.clone(),
        order: order.clone(),
    };
    // The ironmem arm never reaches the CommandRunner; use a no-op FakeRunner.
    let runner = FakeRunner {
        stdout: String::new(),
        ran: Arc::new(Mutex::new(false)),
    };
    let ironmem = Box::new(OrderingIronmemRunner {
        order: order.clone(),
    });
    let exec = LiveExecutor::new(runner, prov, PathBuf::from("/tmp/ws-root"), None)
        .with_ironmem_runner(ironmem);

    let t = task("abcdef1234567890abcdef1234567890abcdef12");
    let out = exec.execute(&t, Arm::Ironmem).unwrap();

    // Verify ordering: provision THEN ironmem_run.
    let log = order.lock().unwrap();
    assert_eq!(*log, vec!["provision", "ironmem_run"], "order was: {log:?}");

    // Verify provisioner was called with the correct base + workspace path.
    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "abcdef1234567890abcdef1234567890abcdef12");
    assert_eq!(
        captured[0].1,
        PathBuf::from("/tmp/ws-root").join("t1").join("ironmem")
    );

    // Verify the ironmem runner's outcome is forwarded as-is.
    assert_eq!(out.usage.total(), 10);
    assert_eq!(out.review_rounds, 1);
    assert_eq!(out.fix_commits, 2);
    assert_eq!(out.outcome, "completed");
}
