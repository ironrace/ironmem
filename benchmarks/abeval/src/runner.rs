//! Drive tasks across arms; write per-task/per-arm artifacts + run_meta.json.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::arms::Arm;
use crate::client::{
    ArmExecutor, ArmOutcome, CommandRunner, DryRunExecutor, LiveExecutor,
    ProcessWorkspaceProvisioner, Usage, WorkspaceProvisioner,
};
use crate::corpus::Task;
use crate::report::{build_arm_metric, TaskMetric};

pub struct RunArgs {
    pub task: Task,
    pub arms: Vec<Arm>,
    pub dry_run: bool,
    pub execute_live: bool,
    pub budget_usd: Option<f64>,
    pub approval_file: Option<PathBuf>,
    pub out_dir: PathBuf,
    /// Run-level base override; used only when a task has no `base_commit`.
    /// Hex refs only. Committed corpus task pins cannot be overridden by the CLI.
    pub base_sha: Option<String>,
}

#[derive(Debug)]
pub struct RunSummary {
    pub task_id: String,
    pub arms_run: usize,
}

#[derive(Serialize)]
struct ArmUsageRecord<'a> {
    arm: &'a str,
    outcome: &'a str,
    usage: &'a Usage,
}

#[derive(Serialize)]
struct RunMeta<'a> {
    task_id: &'a str,
    arms: Vec<&'a str>,
    dry_run: bool,
    approved_paid_run: bool,
    evidence_class: &'a str,
    budget_usd: Option<f64>,
    per_arm: Vec<ArmUsageRecord<'a>>,
}

/// Run one task across the requested arms.
///
/// Dry-run (default) uses `DryRunExecutor` — no network, no model, no spawn.
/// The live path is approval-gated in `run_task_live_guarded`: it refuses before
/// constructing any executor unless BOTH `--execute-live` and cost approval are
/// present; only an approved run spawns.
pub fn run_task(args: RunArgs) -> Result<RunSummary> {
    // Defense-in-depth: the CLI path always validates the corpus first, but a
    // hand-built `RunArgs` could carry an unsafe id. Reject anything that is not
    // basename-safe BEFORE it is joined into an on-disk path (no `..`/separator
    // traversal out of `out_dir`). Checked ahead of BOTH branches so the dry-run
    // and live paths share the guard symmetrically.
    if !crate::corpus::is_safe_task_id(&args.task.id) {
        anyhow::bail!(
            "unsafe task id {:?}: use ASCII letters, digits, '-' or '_' only",
            args.task.id
        );
    }

    // Live path is fully handled by the approval-gated guard.
    if args.execute_live && !args.dry_run {
        return run_task_live_guarded(args);
    }

    let executor = DryRunExecutor;
    let task_dir = args.out_dir.join(&args.task.id);
    std::fs::create_dir_all(&task_dir)
        .with_context(|| format!("creating {}", task_dir.display()))?;

    let mut outcomes: Vec<ArmOutcome> = Vec::new();
    for &arm in &args.arms {
        let outcome = executor.execute(&args.task, arm)?;
        let arm_dir = task_dir.join(arm.label());
        std::fs::create_dir_all(&arm_dir)?;
        atomic_write_json(&arm_dir.join("usage.json"), &outcome.usage)?;
        atomic_write_str(&arm_dir.join("transcript.txt"), &outcome.transcript)?;
        outcomes.push(outcome);
    }

    let meta = RunMeta {
        task_id: &args.task.id,
        arms: args.arms.iter().map(|a| a.label()).collect(),
        dry_run: true,
        approved_paid_run: false,
        evidence_class: "smoke",
        budget_usd: args.budget_usd,
        per_arm: outcomes
            .iter()
            .map(|o| ArmUsageRecord {
                arm: o.arm.label(),
                outcome: &o.outcome,
                usage: &o.usage,
            })
            .collect(),
    };
    atomic_write_json(&task_dir.join("run_meta.json"), &meta)?;

    Ok(RunSummary {
        task_id: args.task.id.clone(),
        arms_run: args.arms.len(),
    })
}

pub fn atomic_write_str(path: &Path, contents: &str) -> Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, contents).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("renaming into {}", path.display()))?;
    Ok(())
}

pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let body = serde_json::to_string_pretty(value)?;
    atomic_write_str(path, &body)
}

/// True iff the paid-run approval opt-in is present in the environment.
pub fn approval_present() -> bool {
    std::env::var(crate::constants::APPROVAL_ENV)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "y" | "approve" | "approved"
            )
        })
        .unwrap_or(false)
}

/// True iff either the env approval or an approval file sentinel is present.
pub fn approval_present_with_file(approval_file: Option<&Path>) -> Result<bool> {
    if approval_present() {
        return Ok(true);
    }
    let Some(path) = approval_file else {
        return Ok(false);
    };
    let body = std::fs::read_to_string(path)
        .with_context(|| format!("reading approval file {}", path.display()))?;
    // Require the sentinel as the exact (trimmed) file content, not merely a
    // substring — the approver must write precisely the opt-in phrase, so the
    // sentinel can't be smuggled in inside unrelated prose.
    Ok(body.trim() == crate::constants::APPROVAL_FILE_SENTINEL)
}

/// Run a task's frozen gates in the produced workspace. Injected so the
/// orchestration can be tested without invoking real `cargo`/`clippy`.
pub trait GateRunner {
    /// True iff every gate for `task` passes in `workspace`.
    fn gates_pass(&self, task: &Task, workspace: &Path) -> Result<bool>;
}

/// Production [`GateRunner`] that runs each of a task's frozen gate strings in
/// the produced workspace as `program + argv`, **without a shell**. The gate set
/// passes iff every gate exits zero; the first non-zero gate fails the set (no
/// silent green).
///
/// No `sh -c`: each gate is split on whitespace into a program and its argument
/// vector and executed directly. This removes the shell-injection class entirely
/// — metacharacters (`;`, `|`, `&&`, redirects) are passed as inert argv tokens,
/// never interpreted. Frozen-corpus gates (`cargo test …`, `cargo clippy …`) do
/// not need shell features; a gate that requires them is intentionally
/// unsupported.
pub struct ProcessGateRunner;

impl GateRunner for ProcessGateRunner {
    fn gates_pass(&self, task: &Task, workspace: &Path) -> Result<bool> {
        for gate in &task.gates {
            let mut tokens = gate.split_whitespace();
            let program = tokens
                .next()
                .ok_or_else(|| anyhow::anyhow!("empty gate string in task {}", task.id))?;
            let argv: Vec<&str> = tokens.collect();

            let status = std::process::Command::new(program)
                .args(&argv)
                .current_dir(workspace)
                .status()
                .with_context(|| format!("running gate {gate:?} in {}", workspace.display()))?;
            if !status.success() {
                // Log the exit code so an environment failure (e.g. exit 127 —
                // the gate program is missing) is visible rather than silently
                // indistinguishable from a genuine red gate.
                let code = status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "signal".to_string());
                eprintln!(
                    "gate failed: {gate:?} exited {code} in {}",
                    workspace.display()
                );
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// Orchestrate one task across `arms`: run each arm through the live executor,
/// run its gates in the produced workspace, and map (agent outcome, gate result)
/// into a per-arm [`TaskMetric`] via the §12 done-proxy. Gates are only run when
/// the agent completed — a failed agent is already not headline-eligible.
pub fn run_task_live<R: CommandRunner, P: WorkspaceProvisioner, G: GateRunner>(
    task: &Task,
    arms: &[Arm],
    executor: &LiveExecutor<R, P>,
    gates: &G,
) -> Result<Vec<TaskMetric>> {
    let mut metrics = Vec::with_capacity(arms.len());
    for &arm in arms {
        let outcome = executor.execute(task, arm)?;
        let ci_green = if outcome.outcome == crate::constants::OUTCOME_COMPLETED {
            let workspace = executor.workspace_for(task, arm);
            gates.gates_pass(task, &workspace)?
        } else {
            false
        };
        metrics.push(build_arm_metric(&task.id, arm.label(), &outcome, ci_green));
    }
    Ok(metrics)
}

/// Run one task across `arms` through the live executor + gates, then write a
/// normalized `evidence_class:"live"` metrics file to
/// `<out_dir>/<task_id>/live_metrics.json` and return its path.
///
/// Generic over the runner/gate seams so it is exercised with fakes; the guarded
/// CLI entry wires the REAL `claude`-spawning runner behind the approval gate.
pub fn execute_approved_live<R: CommandRunner, P: WorkspaceProvisioner, G: GateRunner>(
    task: &Task,
    arms: &[Arm],
    executor: &LiveExecutor<R, P>,
    gates: &G,
    out_dir: &Path,
) -> Result<PathBuf> {
    let metrics = run_task_live(task, arms, executor, gates)?;
    let task_dir = out_dir.join(&task.id);
    std::fs::create_dir_all(&task_dir)
        .with_context(|| format!("creating {}", task_dir.display()))?;
    let path = task_dir.join("live_metrics.json");
    crate::report::write_live_metrics(&path, &metrics)?;
    Ok(path)
}

/// Guard the live path, then (only if approved) run the executor. The approval
/// check happens BEFORE the executor is touched — when `approved` is false this
/// returns an error and the executor is never invoked.
pub fn guard_live_then_run<E: ArmExecutor>(
    task: &Task,
    arms: &[Arm],
    approved: bool,
    executor: &E,
    _out_dir: &Path,
) -> Result<Vec<ArmOutcome>> {
    if !approved {
        anyhow::bail!(
            "live execution requires both --execute-live AND approval via {} \
             (env) or an approval file containing {:?}; refusing to spawn any process",
            crate::constants::APPROVAL_ENV,
            crate::constants::APPROVAL_FILE_SENTINEL
        );
    }
    // Approved: release to the generic executor, one execution per arm, and
    // SURFACE every arm outcome (a failed arm must not be hidden behind a success
    // count). The metrics-writing live entry is `run_task_live_guarded`; this
    // lower-level guard is the release point unit-tested for the guard invariant.
    arms.iter()
        .map(|&arm| executor.execute(task, arm))
        .collect()
}

/// Live entry from `run_task`: build no executor until the guard passes.
pub fn run_task_live_guarded(args: RunArgs) -> Result<RunSummary> {
    let approved = approval_present_with_file(args.approval_file.as_deref())?;
    if !approved {
        anyhow::bail!(
            "live execution requires both --execute-live AND approval via {} \
             (env) or --approval-file containing {:?}; refusing to construct or spawn any process",
            crate::constants::APPROVAL_ENV,
            crate::constants::APPROVAL_FILE_SENTINEL
        );
    }
    // A paid run must carry an explicit cost ceiling: refuse rather than let the
    // documented `--budget-usd` crash-guard be silently absent. (Enforcing the
    // ceiling against per-arm token-derived spend needs the ironmem §7.1 rate
    // table and is tracked as a follow-up; this guarantees the ceiling is at
    // least always present and recorded.)
    if args.budget_usd.is_none() {
        anyhow::bail!(
            "an approved live run requires --budget-usd <amount> as a cost ceiling; \
             refusing to spawn without one"
        );
    }

    // Approved: build the REAL `claude`-spawning runner + shell gate runner and
    // run the task. Arm workspaces live under `<out_dir>/workspaces/...`; the
    // normalized live metrics file is written to `<out_dir>/<task_id>/`.
    //
    // CARGO_MANIFEST_DIR is `<repo>/benchmarks/abeval`; two parent() hops reach
    // the repo root, which is the ironmem repo for worktree provisioning.
    let ironmem_repo = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| std::path::PathBuf::from("."));
    let executor = LiveExecutor::new(
        crate::client::ProcessCommandRunner,
        ProcessWorkspaceProvisioner { ironmem_repo },
        args.out_dir.join("workspaces"),
        args.base_sha.clone(),
    );
    let metrics_path = execute_approved_live(
        &args.task,
        &args.arms,
        &executor,
        &ProcessGateRunner,
        &args.out_dir,
    )?;
    eprintln!("live metrics written to {}", metrics_path.display());
    Ok(RunSummary {
        task_id: args.task.id.clone(),
        arms_run: args.arms.len(),
    })
}
