//! Drive tasks across arms; write per-task/per-arm artifacts + run_meta.json.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::arms::Arm;
use crate::client::{ArmExecutor, ArmOutcome, DryRunExecutor, Usage};
use crate::corpus::Task;

pub struct RunArgs {
    pub task: Task,
    pub arms: Vec<Arm>,
    pub dry_run: bool,
    pub execute_live: bool,
    pub budget_usd: Option<f64>,
    pub out_dir: PathBuf,
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
/// The live path is guarded in Task 5 (`run_task_live_guarded`) before any
/// executor is constructed.
pub fn run_task(args: RunArgs) -> Result<RunSummary> {
    // Live path is fully handled by the Task 5 guard.
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
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Guard the live path, then (only if approved) run the executor. The approval
/// check happens BEFORE the executor is touched — when `approved` is false this
/// returns an error and the executor is never invoked.
pub fn guard_live_then_run<E: ArmExecutor>(
    _task: &Task,
    _arms: &[Arm],
    approved: bool,
    _executor: &E,
    _out_dir: &Path,
) -> Result<RunSummary> {
    if !approved {
        anyhow::bail!(
            "live execution requires both --execute-live AND approval via {} \
             (env) or an approval file; refusing to spawn any process",
            crate::constants::APPROVAL_ENV
        );
    }
    // Approved live execution is intentionally NOT implemented in this PR
    // (no paid runs). Stop at the pre-spawn boundary.
    anyhow::bail!("live execution is not enabled in this PR (no paid runs)")
}

/// Live entry from `run_task`: build no executor until the guard passes.
pub fn run_task_live_guarded(args: RunArgs) -> Result<RunSummary> {
    let approved = approval_present();
    if !approved {
        anyhow::bail!(
            "live execution requires both --execute-live AND approval via {} \
             (env) or an approval file; refusing to construct or spawn any process",
            crate::constants::APPROVAL_ENV
        );
    }
    // Approved but still inert in this PR — no paid runs ship here.
    let _ = args;
    anyhow::bail!("live execution is not enabled in this PR (no paid runs)")
}
