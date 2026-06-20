//! Live-executor behavior (issue #122). All tests inject a fake process or feed
//! canned CLI output — NO real `claude` agent is ever spawned here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use abeval::arms::Arm;
use abeval::client::{
    arm_command, ArmExecutor, ArmOutcome, CommandOutput, CommandRunner, IronmemArmRunner,
    LiveExecutor, ProvisionRequest, Usage, WorkspaceProvisioner,
};
use abeval::corpus::Task;
use abeval::stream_usage::parse_stream_json;

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

// A `--output-format stream-json --verbose` transcript: one assistant message
// carrying the turn's usage, then the terminal `result` event. Summed usage =
// 1200 input / 250 output, so existing per-component assertions are unchanged.
const SUCCESS_JSON: &str = concat!(
    r#"{"type":"assistant","message":{"id":"msg_1","usage":{"input_tokens":1200,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":250}}}"#,
    "\n",
    r#"{"type":"result","is_error":false,"result":"done","usage":{"input_tokens":1200,"cache_creation_input_tokens":0,"cache_read_input_tokens":0,"output_tokens":250}}"#,
);

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

/// Fake ironmem-arm runner: returns a synthetic collab ArmOutcome WITHOUT
/// spawning real git/claude/codex. Lets the executor/aggregation tests drive the
/// ironmem arm under the new contract (it delegates to an IronmemArmRunner).
struct FakeIronmemArm {
    outcome: String,
}
impl IronmemArmRunner for FakeIronmemArm {
    fn run(&self, _task: &Task, _ws: &Path, _out: &Path) -> anyhow::Result<ArmOutcome> {
        Ok(ArmOutcome {
            arm: Arm::Ironmem,
            usage: Usage {
                input_tokens: 1000,
                output_tokens: 200,
                ..Default::default()
            },
            codex_usage: Usage {
                input_tokens: 60,
                cache_read_input_tokens: 40,
                output_tokens: 10,
                cache_creation_input_tokens: 0,
            },
            review_rounds: 2,
            fix_commits: 1,
            outcome: self.outcome.clone(),
            transcript: "fake-collab".to_string(),
        })
    }
}

/// Contract changed (METRICS_SPEC §12 2026-06-17): the ironmem arm drives the
/// headless collab loop via the injected IronmemArmRunner, not a single
/// `claude -p /collab start`; the executor must surface the runner's Claude+Codex
/// ArmOutcome.
#[test]
fn live_executor_ironmem_arm_delegates_to_collab_driver() {
    let (runner, _captured) = fake(SUCCESS_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None)
        .with_ironmem_runner(Box::new(FakeIronmemArm {
            outcome: "completed".into(),
        }));

    let outcome = exec.execute(&task(), Arm::Ironmem).unwrap();

    // The executor surfaces the runner's full ArmOutcome unchanged: Claude side …
    assert_eq!(outcome.outcome, "completed");
    assert_eq!(outcome.usage.input_tokens, 1000);
    // … AND the Codex side + rework counters flow through.
    assert_eq!(outcome.codex_usage.total(), 110);
    assert_eq!(outcome.review_rounds, 2);
    assert_eq!(outcome.fix_commits, 1);
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

/// C1 (METRICS_SPEC §11.2): the superpowers arm must be ENVIRONMENT-isolated
/// from ironmem, not merely prompted. The spawned `claude` must carry
/// `--strict-mcp-config` with an empty `--mcp-config` so it loads ZERO MCP
/// servers and physically cannot reach the ironmem MCP server.
#[test]
fn superpowers_arm_is_mcp_isolated_from_ironmem() {
    let (_program, args) = arm_command(&task(), Arm::Superpowers);

    assert!(
        args.iter().any(|a| a == "--strict-mcp-config"),
        "superpowers arm must pass --strict-mcp-config to ignore inherited MCP config"
    );
    // --mcp-config must be present and its value must declare no servers.
    let cfg_idx = args
        .iter()
        .position(|a| a == "--mcp-config")
        .expect("superpowers arm must pass --mcp-config");
    let cfg_val = args
        .get(cfg_idx + 1)
        .expect("--mcp-config must be followed by a config value");
    let parsed: serde_json::Value =
        serde_json::from_str(cfg_val).expect("--mcp-config value must be valid JSON");
    let servers = parsed
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .expect("config must declare an mcpServers object");
    assert!(
        servers.is_empty(),
        "superpowers arm must declare zero MCP servers (got {servers:?})"
    );
}

/// The ironmem arm intentionally KEEPS the inherited MCP config (it needs the
/// ironmem server for /collab + memory tools), so it must NOT be strict-isolated.
#[test]
fn ironmem_arm_keeps_inherited_mcp_config() {
    let (_program, args) = arm_command(&task(), Arm::Ironmem);
    assert!(
        !args.iter().any(|a| a == "--strict-mcp-config"),
        "ironmem arm must inherit the real MCP config (no --strict-mcp-config)"
    );
}

/// A non-zero exit OR an is_error envelope yields a "failed" outcome (never a
/// silent completion), while still recording the tokens that were spent.
/// The process-exit→failed-outcome behavior is the single-command arm path,
/// which is now the superpowers arm (ironmem drives the collab loop).
#[test]
fn live_executor_failed_process_records_failed_outcome() {
    let (runner, _c) = fake(SUCCESS_JSON, false); // process exited non-zero
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);

    let outcome = exec.execute(&task(), Arm::Superpowers).unwrap();
    assert_eq!(outcome.outcome, "failed");
    assert_eq!(
        outcome.usage.input_tokens, 1200,
        "spent tokens still recorded"
    );
}

/// A stream-json transcript parses into the four §2.1 token components (summed
/// from its assistant messages) plus the terminal `result` event's text/flag.
#[test]
fn parse_stream_json_reads_usage_and_success() {
    let stdout = concat!(
        r#"{"type":"system","subtype":"init","session_id":"abc"}"#,
        "\n",
        r#"{"type":"assistant","message":{"id":"msg_1","usage":{"input_tokens":1200,"cache_creation_input_tokens":300,"cache_read_input_tokens":40,"output_tokens":250}}}"#,
        "\n",
        r#"{"type":"result","subtype":"success","is_error":false,"result":"done","session_id":"abc","total_cost_usd":0.012,"usage":{"input_tokens":1200,"cache_creation_input_tokens":300,"cache_read_input_tokens":40,"output_tokens":250}}"#,
    );

    let parsed = parse_stream_json(stdout).expect("valid transcript parses");

    assert!(!parsed.is_error);
    assert_eq!(parsed.result, "done");
    assert_eq!(parsed.usage.input_tokens, 1200);
    assert_eq!(parsed.usage.cache_creation_input_tokens, 300);
    assert_eq!(parsed.usage.cache_read_input_tokens, 40);
    assert_eq!(parsed.usage.output_tokens, 250);
    // §2.1 tokens_to_done = sum of all four components.
    assert_eq!(parsed.usage.total(), 1200 + 300 + 40 + 250);
}

/// THE point of stream-json (METRICS_SPEC §12 2026-06-19): a Task-subagent's
/// assistant messages run in a separate sub-session that the single-envelope
/// top-level `usage` never rolls up. Summing per-message usage across parent AND
/// subagent ids counts those tokens. The terminal `result` event's own top-level
/// usage (parent-only) must NOT be added on top, or the parent double-counts.
#[test]
fn parse_stream_json_sums_subagent_messages() {
    let stdout = concat!(
        // parent orchestrator turn
        r#"{"type":"assistant","parent_tool_use_id":null,"message":{"id":"msg_parent","usage":{"input_tokens":1000,"output_tokens":200,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        // subagent turn (separate sub-session, distinct message id)
        r#"{"type":"assistant","parent_tool_use_id":"toolu_abc","message":{"id":"msg_sub","usage":{"input_tokens":5000,"output_tokens":800,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        // terminal envelope reports ONLY the parent's roll-up
        r#"{"type":"result","is_error":false,"result":"ok","usage":{"input_tokens":1000,"output_tokens":200,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
    );

    let parsed = parse_stream_json(stdout).expect("transcript parses");
    assert_eq!(
        parsed.usage.input_tokens,
        1000 + 5000,
        "subagent input summed"
    );
    assert_eq!(
        parsed.usage.output_tokens,
        200 + 800,
        "subagent output summed"
    );
    // The parent-only top-level usage (1200) would be a >5x undercount.
    assert_eq!(parsed.usage.total(), 1000 + 200 + 5000 + 800);
}

/// A repeated `message.id` (streamed/partial duplicate) is counted once, at its
/// final usage — last-write-wins per id, never additive double-counting.
#[test]
fn parse_stream_json_dedups_by_message_id() {
    let stdout = concat!(
        r#"{"type":"assistant","message":{"id":"msg_dup","usage":{"input_tokens":10,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"id":"msg_dup","usage":{"input_tokens":300,"output_tokens":50,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"result","is_error":false,"result":"ok","usage":{}}"#,
    );

    let parsed = parse_stream_json(stdout).expect("transcript parses");
    assert_eq!(
        parsed.usage.input_tokens, 300,
        "same id counted once (last wins)"
    );
    assert_eq!(parsed.usage.output_tokens, 50);
}

/// An `is_error: true` terminal event is parsed (not rejected) and surfaces the
/// error flag so the runner records a non-completed outcome rather than crashing.
#[test]
fn parse_stream_json_surfaces_error_event() {
    let stdout = concat!(
        r#"{"type":"assistant","message":{"id":"msg_1","usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"result","subtype":"error_max_turns","is_error":true,"result":"hit limit","usage":{"input_tokens":10,"output_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
    );

    let parsed = parse_stream_json(stdout).expect("error transcript still parses");
    assert!(parsed.is_error);
    assert_eq!(parsed.usage.total(), 15);
}

/// A malformed JSONL line is a loud error, never a silent zero-usage row.
#[test]
fn parse_stream_json_rejects_non_json() {
    let err = parse_stream_json("not json at all").unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("parse")
            || err.to_string().to_lowercase().contains("expected"),
        "non-JSON must surface a parse error, got: {err}"
    );
}

/// A transcript with assistant messages but NO terminal `result` event is schema
/// drift — a loud error, not a silently truncated measurement.
#[test]
fn parse_stream_json_rejects_missing_result_event() {
    let stdout = r#"{"type":"assistant","message":{"id":"m","usage":{"input_tokens":5,"output_tokens":1,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#;
    let err = parse_stream_json(stdout).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("result"),
        "missing terminal result event must be loud, got: {err}"
    );
}

/// A transcript that accumulated REAL subagent usage but then drifted before
/// emitting a terminal `result` must still error — accumulated tokens never leak
/// out as a silent partial measurement row. (Strengthens the guard above past the
/// trivial single-message case.)
#[test]
fn parse_stream_json_missing_result_discards_accumulated_usage() {
    let stdout = concat!(
        r#"{"type":"assistant","message":{"id":"msg_p","usage":{"input_tokens":1000,"output_tokens":200,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"id":"msg_s","usage":{"input_tokens":5000,"output_tokens":800,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
    );
    assert!(
        parse_stream_json(stdout).is_err(),
        "substantial accumulated usage must not leak out without a terminal result event"
    );
}

/// Per-message usage that is ABSENT or a NON-OBJECT (schema drift) contributes 0
/// rather than failing the whole transcript — the well-formed messages still sum.
/// Pins the `unwrap_or_default`/absent-vs-drift fallback so a future change to
/// `?`-on-drift (whole-transcript failure) or to double-counting is caught.
#[test]
fn parse_stream_json_tolerates_absent_and_nonobject_per_message_usage() {
    let stdout = concat!(
        // well-formed
        r#"{"type":"assistant","message":{"id":"ok","usage":{"input_tokens":500,"output_tokens":70,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        // usage key absent entirely
        r#"{"type":"assistant","message":{"id":"no_usage"}}"#,
        "\n",
        // usage present but a non-object (drift) — counted as 0, logged loud
        r#"{"type":"assistant","message":{"id":"bad_usage","usage":"oops"}}"#,
        "\n",
        r#"{"type":"result","is_error":false,"result":"ok","usage":{}}"#,
    );

    let parsed =
        parse_stream_json(stdout).expect("odd per-message usage must not fail the transcript");
    assert_eq!(
        parsed.usage.input_tokens, 500,
        "only the well-formed message contributes"
    );
    assert_eq!(parsed.usage.output_tokens, 70);
}

/// Cache-token fields (`cache_creation_input_tokens` / `cache_read_input_tokens`)
/// must accumulate across MULTIPLE messages — on real collab runs cache tokens
/// dominate the bill. A regression that dropped a cache field in `Usage::add_assign`
/// would pass every single-message test; this asserts each component independently.
#[test]
fn parse_stream_json_sums_cache_fields_across_messages() {
    let stdout = concat!(
        r#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":10,"output_tokens":2,"cache_creation_input_tokens":100,"cache_read_input_tokens":7000}}}"#,
        "\n",
        r#"{"type":"assistant","message":{"id":"b","usage":{"input_tokens":20,"output_tokens":3,"cache_creation_input_tokens":300,"cache_read_input_tokens":9000}}}"#,
        "\n",
        r#"{"type":"result","is_error":false,"result":"ok","usage":{}}"#,
    );

    let parsed = parse_stream_json(stdout).expect("transcript parses");
    assert_eq!(parsed.usage.input_tokens, 10 + 20);
    assert_eq!(parsed.usage.output_tokens, 2 + 3);
    assert_eq!(parsed.usage.cache_creation_input_tokens, 100 + 300);
    assert_eq!(parsed.usage.cache_read_input_tokens, 7000 + 9000);
}

/// Blank/whitespace lines and a trailing newline are tolerated, not treated as
/// malformed-JSON loud errors — real CLI stream-json output ends with a trailing
/// `\n` and can carry blank separator lines.
#[test]
fn parse_stream_json_tolerates_blank_lines_and_trailing_newline() {
    let stdout = concat!(
        "\n",
        r#"{"type":"assistant","message":{"id":"a","usage":{"input_tokens":42,"output_tokens":6,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
        "\n",
        "   \n",
        r#"{"type":"result","is_error":false,"result":"ok","usage":{}}"#,
        "\n",
    );

    let parsed = parse_stream_json(stdout).expect("blank lines and trailing newline are tolerated");
    assert_eq!(parsed.usage.input_tokens, 42);
    assert_eq!(parsed.usage.output_tokens, 6);
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
        codex_usage: Usage::default(),
        review_rounds: 0,
        fix_commits: 0,
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
// ironmem arm driven via FakeIronmemArm (new contract); superpowers via FakeRunner.
#[test]
fn run_task_live_completed_and_green_yields_done_metrics() {
    let (runner, _c) = fake(SUCCESS_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None)
        .with_ironmem_runner(Box::new(FakeIronmemArm {
            outcome: "completed".into(),
        }));
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
// ironmem arm driven via FakeIronmemArm (new contract); gates red → not done.
#[test]
fn run_task_live_red_gates_are_not_done() {
    let (runner, _c) = fake(SUCCESS_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None)
        .with_ironmem_runner(Box::new(FakeIronmemArm {
            outcome: "completed".into(),
        }));
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
        // ironmem arm driven via FakeIronmemArm (new contract); superpowers via FakeRunner.
        let (runner, _c) = fake(SUCCESS_JSON, true);
        let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None)
            .with_ironmem_runner(Box::new(FakeIronmemArm {
                outcome: "completed".into(),
            }));
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
    // ironmem arm driven via FakeIronmemArm (new contract); superpowers via FakeRunner.
    let (runner, _c) = fake(SUCCESS_JSON, true);
    let out = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, out.path().join("ws"), None)
        .with_ironmem_runner(Box::new(FakeIronmemArm {
            outcome: "completed".into(),
        }));
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

// stream-json transcript whose summed assistant usage is zero (schema drift /
// absent usage), so the run-level zero-token guard must fire on success.
const ZERO_USAGE_JSON: &str = concat!(
    r#"{"type":"assistant","message":{"id":"msg_0","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}}"#,
    "\n",
    r#"{"type":"result","is_error":false,"result":"done","usage":{"input_tokens":0,"output_tokens":0,"cache_creation_input_tokens":0,"cache_read_input_tokens":0}}"#,
);

/// A success envelope reporting ZERO total tokens is not physically plausible —
/// it means the usage block was absent/renamed. Recording it as a merged
/// zero-token row would silently deflate the headline cost metric, so it must be
/// a loud error, not a silent measurement.
/// The zero-token loud-error guard lives on the single-`claude -p` arm, which is
/// now the superpowers arm (ironmem drives the collab loop).
#[test]
fn live_executor_zero_usage_on_success_is_loud_error() {
    let (runner, _c) = fake(ZERO_USAGE_JSON, true);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);
    let err = exec.execute(&task(), Arm::Superpowers).unwrap_err();
    assert!(
        err.to_string().to_lowercase().contains("usage")
            || err.to_string().to_lowercase().contains("zero-token"),
        "zero-usage success must be loud, got: {err}"
    );
}

/// Zero usage on a FAILED run is fine (a crashed agent may have spent nothing
/// parseable) — it records a "failed" row, never an error.
/// The zero-token guard lives on the single-`claude -p` arm, which is now the
/// superpowers arm (ironmem drives the collab loop).
#[test]
fn live_executor_zero_usage_on_failure_is_recorded_failed() {
    let (runner, _c) = fake(ZERO_USAGE_JSON, false);
    let ws = tempfile::tempdir().unwrap();
    let exec = LiveExecutor::new(runner, NoOpProvisioner, ws.path().to_path_buf(), None);
    let o = exec.execute(&task(), Arm::Superpowers).unwrap();
    assert_eq!(o.outcome, "failed");
}
