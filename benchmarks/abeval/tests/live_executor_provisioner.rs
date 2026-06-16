//! Tests for the WorkspaceProvisioner seam, base resolution, and LiveExecutor
//! with provision ordering (Task 4).

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use abeval::arms::Arm;
use abeval::client::{
    resolve_base_commit, ArmExecutor, CommandOutput, CommandRunner, LiveExecutor, ProvisionRequest,
    WorkspaceProvisioner,
};
use abeval::corpus::Task;

fn task(base: &str) -> Task {
    Task {
        id: "t1".to_string(),
        title: "T".to_string(),
        source: "issue:#1".to_string(),
        repo_scope: vec!["crates/ironmem/src/lib.rs".to_string()],
        prompt: "p".to_string(),
        acceptance: vec!["a".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
        base_commit: base.to_string(),
    }
}

#[test]
fn base_resolution_task_pin_wins() {
    let t = task("abcdef1234567890abcdef1234567890abcdef12");
    let resolved = resolve_base_commit(&t, Some("0000000")).unwrap();
    assert_eq!(resolved, "abcdef1234567890abcdef1234567890abcdef12");
}

#[test]
fn base_resolution_run_override_used_when_task_empty() {
    let t = task("");
    let resolved = resolve_base_commit(&t, Some("0123456")).unwrap();
    assert_eq!(resolved, "0123456");
}

#[test]
fn base_resolution_both_absent_fails_loud() {
    let t = task("");
    let err = resolve_base_commit(&t, None).unwrap_err().to_string();
    assert!(err.contains("refusing to provision"), "got: {err}");
    assert!(err.contains("t1"), "got: {err}");
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
    let out = exec.execute(&t, Arm::Ironmem).unwrap();
    assert_eq!(out.usage.total(), 15);
    let captured = calls.lock().unwrap();
    assert_eq!(captured.len(), 1);
    assert_eq!(captured[0].0, "abcdef1234567890abcdef1234567890abcdef12");
    assert_eq!(
        captured[0].1,
        PathBuf::from("/tmp/ws-root").join("t1").join("ironmem")
    );
    assert!(*ran.lock().unwrap(), "runner.run must be called");
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
