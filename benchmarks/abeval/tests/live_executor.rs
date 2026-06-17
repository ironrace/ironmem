//! Live-executor behavior (issue #122). All tests inject a fake process or feed
//! canned CLI output — NO real `claude` agent is ever spawned here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use abeval::arms::Arm;
use abeval::client::{
    parse_cli_result, ArmExecutor, ArmOutcome, CommandOutput, CommandRunner, LiveExecutor,
    ProvisionRequest, Usage, WorkspaceProvisioner,
};
use abeval::corpus::Task;

/// No-op provisioner for tests that don't test provisioning behavior — just
/// creates the workspace directory so the runner can write artifacts into it.
struct NoOpProvisioner;

impl WorkspaceProvisioner for NoOpProvisioner {
    fn provision(&self, req: &ProvisionRequest) -> anyhow::Result<()> {
        std::fs::create_dir_all(req.workspace)?;
        Ok(())
    }
}
use abeval::report::{build_arm_metric, load_metrics, render_report, write_live_metrics};

fn task() -> Task {
    Task {
        id: "abeval-01-issue-95".to_string(),
        title: "rerank gate".to_string(),
        source: "issue:#95".to_string(),
        repo_scope: vec!["crates/ironmem/**".to_string()],
        prompt: "PROMPT-BODY".to_string(),
        acceptance: vec!["ok".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
        base_commit: abeval::corpus::BaseCommit::parse("ce2b27f2bcf3d318e0142ff5a1ece578559d9261")
            .unwrap(),
    }
}

const SUCCESS_JSON: &str = r#"{
    "type": "result", "is_error": false, "result": "done",
    "usage": {"input_tokens": 1200, "cache_creation_input_tokens": 0,
              "cache_read_input_tokens": 0, "output_tokens": 250}
}"#;

type Captured = Arc<Mutex<Vec<(String, Vec<String>, PathBuf)>>>;

/// Records every command it is asked to run and returns canned output. Never
/// spawns a process.
struct FakeRunner {
    stdout: String,
    success: bool,
    captured: Captured,
}

impl CommandRunner for FakeRunner {
    fn run(
        &self,
        program: &str,
        args: &[String],
        workspace: &Path,
    ) -> anyhow::Result<CommandOutput> {
        self.captured.lock().unwrap().push((
            program.to_string(),
            args.to_vec(),
            workspace.to_path_buf(),
        ));
        Ok(CommandOutput {
            stdout: self.stdout.clone(),
            success: self.success,
        })
    }
}

fn fake(stdout: &str, success: bool) -> (FakeRunner, Captured) {
    let captured: Captured = Arc::new(Mutex::new(Vec::new()));
    (
        FakeRunner {
            stdout: stdout.to_string(),
            success,
            captured: captured.clone(),
        },
        captured,
    )
}

/// The ironmem arm drives `claude -p "/collab start <prompt>"` and the parsed
/// usage + success flow to a "completed" outcome.
#[test]
fn live_executor_ironmem_arm_runs_collab_and_parses_usage() {
    let (runner, captured) = fake(SUCCESS_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);

    let outcome = exec.execute(&task(), Arm::Ironmem).unwrap();

    assert_eq!(outcome.outcome, "completed");
    assert_eq!(outcome.usage.input_tokens, 1200);
    assert_eq!(outcome.usage.total(), 1450);

    let cap = captured.lock().unwrap();
    assert_eq!(cap.len(), 1);
    assert_eq!(cap[0].0, "claude");
    // `/collab start` is carried inside the single `-p` prompt arg (print mode).
    assert!(
        cap[0].1.iter().any(|a| a.contains("/collab start")),
        "ironmem arm uses /collab start (inside -p prompt)"
    );
    assert!(
        cap[0].1.iter().any(|a| a.contains("PROMPT-BODY")),
        "task prompt is passed"
    );
}

/// The superpowers arm runs the prompt with the skills-only prefix and NEVER
/// invokes /collab.
#[test]
fn live_executor_superpowers_arm_excludes_collab() {
    let (runner, captured) = fake(SUCCESS_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);

    exec.execute(&task(), Arm::Superpowers).unwrap();

    let cap = captured.lock().unwrap();
    let args = &cap[0].1;
    assert!(
        args.iter().any(|a| a == "-p"),
        "superpowers arm uses headless -p"
    );
    // The contract is "never INVOKE /collab" — i.e. no arg is the `/collab`
    // command token. The skills-only prefix text legitimately mentions "/collab"
    // in its prohibition ("Do not use /collab"), so a substring check would be
    // wrong; assert on the command token instead.
    assert!(
        !args.iter().any(|a| a == "/collab"),
        "superpowers arm must never invoke the /collab command"
    );
    assert!(
        args.iter().any(|a| a.contains("superpowers skills only")),
        "skills-only prefix is applied"
    );
    assert!(args.iter().any(|a| a.contains("PROMPT-BODY")));
}

/// A non-zero exit OR an is_error envelope yields a "failed" outcome (never a
/// silent completion), while still recording the tokens that were spent.
#[test]
fn live_executor_failed_process_records_failed_outcome() {
    let (runner, _c) = fake(SUCCESS_JSON, false); // process exited non-zero
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);

    let outcome = exec.execute(&task(), Arm::Ironmem).unwrap();
    assert_eq!(outcome.outcome, "failed");
    assert_eq!(
        outcome.usage.input_tokens, 1200,
        "spent tokens still recorded"
    );
}

/// The `claude -p --output-format json` success envelope parses into the four
/// §2.1 token components plus the success outcome.
#[test]
fn parse_cli_result_reads_usage_and_success() {
    let stdout = r#"{
        "type": "result",
        "subtype": "success",
        "is_error": false,
        "result": "done",
        "session_id": "abc",
        "total_cost_usd": 0.012,
        "usage": {
            "input_tokens": 1200,
            "cache_creation_input_tokens": 300,
            "cache_read_input_tokens": 40,
            "output_tokens": 250
        }
    }"#;

    let parsed = parse_cli_result(stdout).expect("valid envelope parses");

    assert!(!parsed.is_error);
    assert_eq!(parsed.result, "done");
    assert_eq!(parsed.usage.input_tokens, 1200);
    assert_eq!(parsed.usage.cache_creation_input_tokens, 300);
    assert_eq!(parsed.usage.cache_read_input_tokens, 40);
    assert_eq!(parsed.usage.output_tokens, 250);
    // §2.1 tokens_to_done = sum of all four components.
    assert_eq!(parsed.usage.total(), 1200 + 300 + 40 + 250);
}

/// An `is_error: true` envelope is parsed (not rejected) and surfaces the error
/// flag so the runner can record a non-completed outcome rather than crashing.
#[test]
fn parse_cli_result_surfaces_error_envelope() {
    let stdout = r#"{
        "type": "result",
        "subtype": "error_max_turns",
        "is_error": true,
        "result": "hit limit",
        "usage": {"input_tokens": 10, "output_tokens": 5,
                  "cache_creation_input_tokens": 0, "cache_read_input_tokens": 0}
    }"#;

    let parsed = parse_cli_result(stdout).expect("error envelope still parses");
    assert!(parsed.is_error);
    assert_eq!(parsed.usage.total(), 15);
}

/// Non-JSON / truncated CLI output is a loud error, never a silent zero-usage row.
#[test]
fn parse_cli_result_rejects_non_json() {
    let err = parse_cli_result("not json at all").unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("parse")
            || err.to_string().to_lowercase().contains("expected"),
        "non-JSON must surface a parse error, got: {err}"
    );
}

fn arm_outcome(outcome: &str, input: u32) -> ArmOutcome {
    ArmOutcome {
        arm: Arm::Ironmem,
        usage: Usage {
            input_tokens: input,
            output_tokens: 0,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
        },
        outcome: outcome.to_string(),
        transcript: String::new(),
    }
}

/// Agent completed AND gates green → the merged-proxy: outcome "merged",
/// ci_green, and headline-eligible (is_done()).
#[test]
fn build_arm_metric_completed_and_green_is_merged_and_done() {
    let m = build_arm_metric("abeval-01", "ironmem", &arm_outcome("completed", 500), true);
    assert_eq!(m.arm, "ironmem");
    assert_eq!(m.task_key, "abeval-01:ironmem");
    assert_eq!(m.outcome, "merged");
    assert!(m.ci_green);
    assert!(!m.estimated, "live rows are measured, never estimated");
    assert_eq!(m.input_tokens, 500);
    assert!(
        m.is_done(),
        "merged + ci_green + measured counts toward headline"
    );
}

/// Agent completed but gates RED → not merged, not done (no silent pass).
#[test]
fn build_arm_metric_completed_but_red_is_not_done() {
    let m = build_arm_metric(
        "abeval-01",
        "ironmem",
        &arm_outcome("completed", 500),
        false,
    );
    assert!(!m.ci_green);
    assert_ne!(m.outcome, "merged");
    assert!(!m.is_done());
}

/// Agent failed → never done regardless of gate result.
#[test]
fn build_arm_metric_failed_agent_is_not_done() {
    let m = build_arm_metric("abeval-01", "ironmem", &arm_outcome("failed", 500), true);
    assert_eq!(m.outcome, "failed");
    assert!(!m.is_done());
}

/// `write_live_metrics` produces a file that `load_metrics` reads as live
/// evidence and that `render_report` turns into a §11.3 DELTA when each arm has
/// 8 merged+green tasks.
#[test]
fn write_live_metrics_roundtrips_to_a_headline_delta() {
    let mut tasks = Vec::new();
    for arm in ["ironmem", "superpowers"] {
        let base = if arm == "ironmem" { 100 } else { 200 };
        for i in 0..8 {
            tasks.push(build_arm_metric(
                &format!("abeval-{i:02}"),
                arm,
                &arm_outcome("completed", base + i),
                true,
            ));
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("live_metrics.json");
    write_live_metrics(&path, &tasks).unwrap();

    let loaded = load_metrics(&path).unwrap();
    assert_eq!(loaded.evidence_class, "live");
    let report = render_report(&loaded);
    assert!(
        report.contains("DELTA"),
        "8/arm merged+green must clear the gate:\n{report}"
    );
}

use abeval::runner::{run_task_live, GateRunner};

struct FakeGates {
    green: bool,
}
impl GateRunner for FakeGates {
    fn gates_pass(&self, _task: &Task, _workspace: &Path) -> anyhow::Result<bool> {
        Ok(self.green)
    }
}

fn task_n(n: usize) -> Task {
    Task {
        id: format!("abeval-{n:02}"),
        title: "t".to_string(),
        source: "issue:#95".to_string(),
        repo_scope: vec!["crates/**".to_string()],
        prompt: format!("PROMPT-{n}"),
        acceptance: vec!["ok".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
        base_commit: abeval::corpus::BaseCommit::parse("ce2b27f2bcf3d318e0142ff5a1ece578559d9261")
            .unwrap(),
    }
}

/// run_task_live drives every arm through the executor, runs gates per arm, and
/// returns merged+done metrics when the agent completes and gates are green.
#[test]
fn run_task_live_completed_and_green_yields_done_metrics() {
    let (runner, _c) = fake(SUCCESS_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);
    let gates = FakeGates { green: true };

    let metrics = run_task_live(&task(), &[Arm::Ironmem, Arm::Superpowers], &exec, &gates).unwrap();

    assert_eq!(metrics.len(), 2);
    assert!(
        metrics.iter().all(|m| m.is_done()),
        "both arms merged+green"
    );
    assert!(metrics.iter().any(|m| m.arm == "ironmem"));
    assert!(metrics.iter().any(|m| m.arm == "superpowers"));
}

/// Gates RED → arms are attempted but not done (no silent merge).
#[test]
fn run_task_live_red_gates_are_not_done() {
    let (runner, _c) = fake(SUCCESS_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);
    let gates = FakeGates { green: false };

    let metrics = run_task_live(&task(), &[Arm::Ironmem], &exec, &gates).unwrap();
    assert!(!metrics[0].is_done());
}

/// End-to-end with fakes: 8 tasks × 2 arms through run_task_live, aggregated and
/// written via write_live_metrics, reload + render → a §11.3 DELTA. NO real agent.
#[test]
fn full_pipeline_eight_tasks_produces_headline_delta() {
    let ws = tempfile::tempdir().unwrap();
    let gates = FakeGates { green: true };
    let mut all = Vec::new();
    for n in 0..8 {
        // Distinct usage per arm so spreads are meaningful; success envelope.
        let (runner, _c) = fake(SUCCESS_JSON, true);
        let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);
        let metrics =
            run_task_live(&task_n(n), &[Arm::Ironmem, Arm::Superpowers], &exec, &gates).unwrap();
        all.extend(metrics);
    }
    let path = ws.path().join("aggregated_live.json");
    write_live_metrics(&path, &all).unwrap();

    let loaded = load_metrics(&path).unwrap();
    let report = render_report(&loaded);
    assert!(
        report.contains("DELTA"),
        "8/arm merged+green clears the gate:\n{report}"
    );
}

/// The approved orchestration writes a normalized `evidence_class:"live"`
/// metrics file under the out dir and returns its path. Driven with fakes — no
/// real agent. (The guarded CLI entry wires the REAL runner behind the approval
/// gate; this proves the write/aggregate logic the entry delegates to.)
#[test]
fn execute_approved_live_writes_live_metrics_file() {
    let (runner, _c) = fake(SUCCESS_JSON, true);
    let out = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, out.path().join("ws"), None);
    let gates = FakeGates { green: true };

    let path = abeval::runner::execute_approved_live(
        &task(),
        &[Arm::Ironmem, Arm::Superpowers],
        &exec,
        &gates,
        out.path(),
    )
    .unwrap();

    assert!(path.exists(), "metrics file written");
    let loaded = load_metrics(&path).unwrap();
    assert_eq!(loaded.evidence_class, "live");
    assert_eq!(loaded.tasks.len(), 2);
    assert!(loaded.tasks.iter().all(|t| t.is_done()));
}

// --- Cycle 5: real subprocess plumbing, proven with harmless coreutils only ---

use abeval::client::ProcessCommandRunner;
use abeval::runner::ProcessGateRunner;

/// The real runner spawns a process, captures its stdout, and reports success.
/// Uses `printf` — NEVER an agent; safe to run in CI.
#[test]
fn process_command_runner_captures_stdout_and_success() {
    let runner = ProcessCommandRunner;
    let ws = tempfile::tempdir().unwrap();
    let out = runner
        .run(
            "printf",
            &["%s".to_string(), "hello-stdout".to_string()],
            ws.path(),
        )
        .unwrap();
    assert!(out.success);
    assert_eq!(out.stdout, "hello-stdout");
}

/// A non-zero exit is reported as `success = false` (not an Err).
/// Uses `false` — NEVER an agent; safe to run in CI.
#[test]
fn process_command_runner_reports_failure_exit() {
    let runner = ProcessCommandRunner;
    let ws = tempfile::tempdir().unwrap();
    let out = runner.run("false", &[], ws.path()).unwrap();
    assert!(!out.success);
}

/// A missing program is a loud error, not a silent empty success.
/// Spawns no real workload — only a non-existent binary; safe to run in CI.
#[test]
fn process_command_runner_errors_on_missing_program() {
    let runner = ProcessCommandRunner;
    let ws = tempfile::tempdir().unwrap();
    assert!(runner
        .run("abeval-no-such-binary-xyz", &[], ws.path())
        .is_err());
}

/// ProcessGateRunner runs each gate (program + argv, no shell) in the workspace;
/// all-zero-exit passes. Uses `true` — NEVER an agent; safe to run in CI.
#[test]
fn process_gate_runner_passes_when_all_gates_succeed() {
    let g = ProcessGateRunner;
    let ws = tempfile::tempdir().unwrap();
    let mut t = task();
    t.gates = vec!["true".to_string(), "true".to_string()];
    assert!(g.gates_pass(&t, ws.path()).unwrap());
}

/// Any non-zero gate fails the set (no silent green).
/// Uses `true`/`false` — NEVER an agent; safe to run in CI.
#[test]
fn process_gate_runner_fails_when_a_gate_fails() {
    let g = ProcessGateRunner;
    let ws = tempfile::tempdir().unwrap();
    let mut t = task();
    t.gates = vec!["true".to_string(), "false".to_string()];
    assert!(!g.gates_pass(&t, ws.path()).unwrap());
}

/// SECURITY: gates run WITHOUT a shell — metacharacters in a gate string are
/// passed as inert argv tokens, never interpreted. With `sh -c`, `true ; touch X`
/// would create X; here the `; touch X` tokens are just args to `true`.
/// Uses `true` — NEVER an agent; safe to run in CI (regression guard for #122).
#[test]
fn process_gate_runner_does_not_interpret_shell_metacharacters() {
    let g = ProcessGateRunner;
    let ws = tempfile::tempdir().unwrap();
    let marker = ws.path().join("PWNED");
    let mut t = task();
    t.gates = vec![format!("true ; touch {}", marker.display())];

    // `true` ignores its args and exits 0; the injection never executes.
    assert!(g.gates_pass(&t, ws.path()).unwrap());
    assert!(
        !marker.exists(),
        "shell metacharacters must not be interpreted (no shell)"
    );
}

/// An empty / whitespace-only gate string is malformed → loud error, not a
/// silent pass.
#[test]
fn process_gate_runner_rejects_empty_gate() {
    let g = ProcessGateRunner;
    let ws = tempfile::tempdir().unwrap();
    let mut t = task();
    t.gates = vec!["   ".to_string()];
    assert!(g.gates_pass(&t, ws.path()).is_err());
}

// --- Review fixes (issue #122 review round) ---

const ZERO_USAGE_JSON: &str = r#"{"is_error":false,"result":"done",
    "usage":{"input_tokens":0,"output_tokens":0,
             "cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#;

/// A success envelope reporting ZERO total tokens is not physically plausible —
/// it means the usage block was absent/renamed. Recording it as a merged
/// zero-token row would silently deflate the headline cost metric, so it must be
/// a loud error, not a silent measurement.
#[test]
fn live_executor_zero_usage_on_success_is_loud_error() {
    let (runner, _c) = fake(ZERO_USAGE_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);
    let err = exec.execute(&task(), Arm::Ironmem).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("usage")
            || err.to_string().to_lowercase().contains("zero-token"),
        "zero-usage success must be loud, got: {err}"
    );
}

/// Zero usage on a FAILED run is fine (a crashed agent may have spent nothing
/// parseable) — it records a "failed" row, never an error.
#[test]
fn live_executor_zero_usage_on_failure_is_recorded_failed() {
    let (runner, _c) = fake(ZERO_USAGE_JSON, false);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);
    let o = exec.execute(&task(), Arm::Ironmem).unwrap();
    assert_eq!(o.outcome, "failed");
}
