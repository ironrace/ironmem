//! Execution abstraction: `Usage` (same field shape as the provbench baseline
//! client), the `ArmExecutor` trait, a deterministic `DryRunExecutor`, the
//! `CommandRunner` seam (`ProcessCommandRunner` for production / a fake in
//! tests), and the `LiveExecutor` that runs an arm and parses the CLI envelope.
//! A real `claude` spawn is reached only through the approval-gated live entry
//! in `runner::run_task_live_guarded`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::arms::Arm;
use crate::corpus::{BaseCommit, Task};

const SUPERPOWERS_PROMPT_PREFIX: &str = "Run this task with superpowers skills only. \
Do not use /collab, ironmem MCP tools, semantic search, KG reads/writes, drawer \
reads/writes, or any ironmem server-side memory state in the working context. \
Passive measurement-only task tagging must stay outside the task-solving path.\n\nTask:\n";

/// Token accounting — the four §2.1 components. Shape reused verbatim from
/// `benchmarks/provbench/baseline/src/client.rs`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub input_tokens: u32,
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    #[serde(default)]
    pub cache_read_input_tokens: u32,
    #[serde(default)]
    pub output_tokens: u32,
}

impl Usage {
    /// Saturating field-wise accumulation.
    pub fn add_assign(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.cache_read_input_tokens = self
            .cache_read_input_tokens
            .saturating_add(other.cache_read_input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
    }

    /// §2.1 tokens_to_done = input + output + cache_creation + cache_read.
    pub fn total(&self) -> u64 {
        self.input_tokens as u64
            + self.output_tokens as u64
            + self.cache_creation_input_tokens as u64
            + self.cache_read_input_tokens as u64
    }
}

/// Outcome of running one task in one arm.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArmOutcome {
    pub arm: Arm,
    /// Claude-side token usage (the driving CLI envelope, or summed across collab
    /// worker turns for the ironmem arm).
    pub usage: Usage,
    /// Codex-side token usage (ironmem arm only; zero for superpowers). Attributed
    /// from `~/.codex/sessions` rollouts by worktree cwd + window.
    #[serde(default)]
    pub codex_usage: Usage,
    /// §11.4 rework counters (ironmem arm only; zero for superpowers).
    #[serde(default)]
    pub review_rounds: u32,
    #[serde(default)]
    pub fix_commits: u32,
    /// `"completed"`/`"failed"` (agent-level). The §12 done-proxy lifts this to
    /// `"merged"` only when gates are green (in `build_arm_metric`).
    pub outcome: String,
    pub transcript: String,
}

pub trait ArmExecutor {
    fn execute(&self, task: &Task, arm: Arm) -> Result<ArmOutcome>;
}

/// Parsed `claude -p --output-format json` result envelope. Only the fields the
/// harness needs are read; unknown fields are ignored.
#[derive(Debug, Clone)]
pub struct CliResult {
    pub is_error: bool,
    pub result: String,
    pub usage: Usage,
}

#[derive(Deserialize)]
struct CliEnvelope {
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    result: String,
    #[serde(default)]
    usage: Usage,
}

/// Parse the `claude -p --output-format json` envelope into a [`CliResult`].
///
/// The error flag is surfaced (not rejected) so the runner can record a
/// non-completed outcome; malformed/non-JSON output is a loud error rather than
/// a silent zero-usage row.
pub fn parse_cli_result(stdout: &str) -> Result<CliResult> {
    let env: CliEnvelope = serde_json::from_str(stdout)
        .map_err(|e| anyhow::anyhow!("failed to parse claude CLI JSON envelope: {e}"))?;
    Ok(CliResult {
        is_error: env.is_error,
        result: env.result,
        usage: env.usage,
    })
}

/// Deterministic, network-free executor used for the committed smoke path.
pub struct DryRunExecutor;

impl ArmExecutor for DryRunExecutor {
    fn execute(&self, task: &Task, arm: Arm) -> Result<ArmOutcome> {
        // Deterministic synthesized usage derived from stable task/arm bytes.
        let seed = task.id.len() as u32 + arm.label().len() as u32;
        let usage = Usage {
            input_tokens: 1000 + seed,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            output_tokens: 200 + seed,
        };
        Ok(ArmOutcome {
            arm,
            usage,
            codex_usage: Usage::default(),
            review_rounds: 0,
            fix_commits: 0,
            outcome: "completed".to_string(),
            transcript: format!("[dry-run] {} :: {}", arm.label(), task.id),
        })
    }
}

/// Single source of truth for the headless permission tokens both arms carry,
/// so a future CLI change is a one-line edit and the arms stay in lockstep.
/// Fallback form if the deployed CLI lacks `--permission-mode`:
/// `["--dangerously-skip-permissions"]`.
fn headless_permission_args() -> Vec<String> {
    vec![
        "--permission-mode".to_string(),
        "bypassPermissions".to_string(),
    ]
}

/// C1 (METRICS_SPEC §11.2) environment isolation for the `superpowers` arm.
///
/// `--strict-mcp-config` makes the CLI ignore every inherited MCP configuration
/// and use ONLY servers from the accompanying `--mcp-config`; that config
/// declares zero servers. The result is that the arm physically cannot reach the
/// ironmem MCP server (search/KG/drawers) no matter what the prompt does — the
/// prompt prefix is belt-and-suspenders only. Single source of truth so a CLI
/// flag change is a one-line edit, mirroring [`headless_permission_args`].
fn superpowers_mcp_isolation_args() -> Vec<String> {
    vec![
        "--strict-mcp-config".to_string(),
        "--mcp-config".to_string(),
        r#"{"mcpServers":{}}"#.to_string(),
    ]
}

/// Build the command for an arm. Returns the program and args that are spawned
/// by a [`CommandRunner`].
///
/// - `ironmem` arm: starts a `/collab` flow for the task (carried INSIDE the
///   print-mode `-p` prompt string so the command is genuinely non-interactive
///   and JSON-emitting).
/// - `superpowers` arm: runs the task prompt with superpowers skills ONLY
///   (C1: NO `/collab`, NO semantic search/KG/drawer writes, NO ironmem
///   server-side state in the working context). This arm is ENVIRONMENT-isolated
///   via [`superpowers_mcp_isolation_args`] (`--strict-mcp-config` + empty
///   `--mcp-config`), so it cannot reach the ironmem MCP server even if the model
///   ignores the prompt prefix. Any task_tag/reporting instrumentation is
///   measurement-only and kept out of the working path.
///
/// Both arms request `--output-format json` AND `-p` (print mode, required for
/// `--output-format` to take effect) AND headless permission tokens. The
/// isolation flags precede `-p` so the prompt remains the print-mode positional.
///
/// NOTE: as of METRICS_SPEC §12 2026-06-17, `LiveExecutor::execute` no longer
/// calls `arm_command` for `Arm::Ironmem` — that arm delegates to an
/// `IronmemArmRunner` (the headless collab driver). The `Arm::Ironmem` branch
/// here is retained only for the `arm_command` unit tests / `arms.rs`.
pub fn arm_command(task: &Task, arm: Arm) -> (String, Vec<String>) {
    let mut argv = vec!["--output-format".to_string(), "json".to_string()];
    argv.extend(headless_permission_args());
    // C1 (§11.2): the superpowers arm loads ZERO MCP servers so it physically
    // cannot reach ironmem; the ironmem arm keeps the inherited config because it
    // needs that server for /collab + memory tools. Added before `-p` so the
    // prompt stays the print-mode positional argument.
    if matches!(arm, Arm::Superpowers) {
        argv.extend(superpowers_mcp_isolation_args());
    }
    argv.push("-p".to_string());
    match arm {
        Arm::Ironmem => {
            // `/collab start` preserved INSIDE the print-mode prompt string so
            // the command is genuinely non-interactive and JSON-emitting.
            argv.push(format!("/collab start {}", task.prompt));
        }
        Arm::Superpowers => {
            argv.push(format!("{SUPERPOWERS_PROMPT_PREFIX}{}", task.prompt));
        }
    }
    ("claude".to_string(), argv)
}

/// Output of running one arm command.
pub struct CommandOutput {
    pub stdout: String,
    /// True iff the process exited with a success status.
    pub success: bool,
}

/// Abstraction over running an arm's external command. Injected so the executor
/// can be exercised with a fake — the production impl spawns a real process,
/// tests never do.
pub trait CommandRunner {
    fn run(&self, program: &str, args: &[String], workspace: &Path) -> Result<CommandOutput>;
}

/// Production [`CommandRunner`] that spawns a real process in the given
/// workspace and captures its stdout. The only caller that reaches a real
/// `claude` spawn is the approval-gated live entry in `runner`.
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, program: &str, args: &[String], workspace: &Path) -> Result<CommandOutput> {
        let output = std::process::Command::new(program)
            .args(args)
            .current_dir(workspace)
            .output()
            .with_context(|| format!("spawning {program} in {}", workspace.display()))?;
        Ok(CommandOutput {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            success: output.status.success(),
        })
    }
}

/// Runs the ironmem arm (a full headless `/collab` flow) for one task. Injected
/// into [`LiveExecutor`] so the heavy real-process path
/// (`collab_live::run_ironmem_arm`) can be replaced by a fake in tests —
/// mirroring [`CommandRunner`]/[`WorkspaceProvisioner`]. The ironmem arm no
/// longer runs a single `claude -p "/collab start"`; it drives the dispatcher
/// loop (METRICS_SPEC §12 2026-06-17).
pub trait IronmemArmRunner {
    fn run(&self, task: &Task, workspace: &Path, out_task_dir: &Path) -> Result<ArmOutcome>;
}

/// Production [`IronmemArmRunner`]: drives the real collab loop. Reached only
/// behind the approval gate (it spawns real processes).
pub struct ProcessIronmemArmRunner;

impl IronmemArmRunner for ProcessIronmemArmRunner {
    fn run(&self, task: &Task, workspace: &Path, out_task_dir: &Path) -> Result<ArmOutcome> {
        crate::collab_live::run_ironmem_arm(task, workspace, out_task_dir)
    }
}

/// Inputs to provisioning one arm's workspace.
pub struct ProvisionRequest<'a> {
    pub task: &'a Task,
    pub arm: Arm,
    pub base_commit: &'a BaseCommit,
    pub workspace_root: &'a Path,
    pub workspace: &'a Path,
}

/// Abstraction over creating a populated per-task/arm workspace. Production does
/// a real `git worktree add`; tests use a fake. Mirrors [`CommandRunner`].
pub trait WorkspaceProvisioner {
    fn provision(&self, req: &ProvisionRequest) -> Result<()>;
}

/// Resolve the base commit for a task. THE single authority on base-commit
/// precedence:
///
/// - BOTH a (validated) task pin AND a non-empty run override present → error.
///   Overriding a deliberate corpus pin from the CLI is illegal; the corpus pin
///   must be edited intentionally instead.
/// - Task pin present (and valid) → use it.
/// - No task pin but a run override present → validate and use the override.
/// - Neither present → error (no silent HEAD fallback).
/// - Either value present but invalid → error with a value-specific message.
///
/// `task.base_commit` is already a validated [`BaseCommit`]; an empty inner ref
/// (only reachable via [`BaseCommit::unset`] for hand-built run-override tasks)
/// means "no task pin". The run override is the one value still arriving as a
/// raw string, so it is validated here via [`BaseCommit::parse`].
pub fn resolve_base_commit(task: &Task, run_override: Option<&str>) -> Result<BaseCommit> {
    let has_pin = !task.base_commit.as_str().trim().is_empty();
    let override_raw = run_override.map(str::trim).filter(|s| !s.is_empty());

    match (has_pin, override_raw) {
        (true, Some(sha)) => anyhow::bail!(
            "--base-sha {:?} cannot override pinned base_commit for task {}; \
             edit the corpus pin intentionally instead",
            sha,
            task.id
        ),
        // The pin is an already-validated `BaseCommit`; clone it through.
        (true, None) => Ok(task.base_commit.clone()),
        (false, Some(sha)) => {
            BaseCommit::parse(sha).map_err(|e| anyhow::anyhow!("--base-sha is {e}"))
        }
        (false, None) => anyhow::bail!(
            "task {} has no base_commit and no --base-sha was provided; \
             refusing to provision a workspace at an undefined base",
            task.id
        ),
    }
}

/// Pure builder for the `git worktree add` command (no spawn). Exposed so the
/// argv can be unit-tested without touching real git.
pub fn worktree_add_argv(
    repo: &std::path::Path,
    workspace: &std::path::Path,
    base: &str,
) -> (String, Vec<String>) {
    (
        "git".to_string(),
        vec![
            "-C".to_string(),
            repo.display().to_string(),
            "worktree".to_string(),
            "add".to_string(),
            "--detach".to_string(),
            "--".to_string(),
            workspace.display().to_string(),
            base.to_string(),
        ],
    )
}

/// The parent directory of the arm workspace leaf (`<root>/<task_id>` for a
/// `<root>/<task_id>/<arm>` workspace). The caller creates it via
/// `create_dir_all` before `git worktree add`, because `git worktree add` does
/// not create missing intermediate parents.
pub fn worktree_parent_dir(workspace: &std::path::Path) -> Option<PathBuf> {
    workspace.parent().map(Path::to_path_buf)
}

/// Enforce that the arm workspace is a safe leaf under the workspace root.
/// Three guards, each failing loud with a distinct message:
///
/// 1. Containment / under-root: `workspace` must be a descendant of
///    `workspace_root` (`strip_prefix` must succeed).
/// 2. `..`/root escape: the relative path must contain no `ParentDir`
///    (`..`) or `RootDir` component that would climb out of the root.
/// 3. Symlink: neither the workspace root nor any intermediate path component
///    down to the arm workspace may be a symlink.
///
/// Live runs write into user-supplied `--out`; this prevents a reused/shared
/// output tree from redirecting worktree creation outside the intended root.
pub fn ensure_workspace_path_safe(workspace_root: &Path, workspace: &Path) -> Result<()> {
    let rel = workspace.strip_prefix(workspace_root).with_context(|| {
        format!(
            "workspace {} is not under workspace root {}",
            workspace.display(),
            workspace_root.display()
        )
    })?;
    if rel.components().any(|c| {
        matches!(
            c,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )
    }) {
        anyhow::bail!(
            "workspace {} escapes workspace root {}",
            workspace.display(),
            workspace_root.display()
        );
    }

    if let Ok(meta) = std::fs::symlink_metadata(workspace_root) {
        if meta.file_type().is_symlink() {
            anyhow::bail!(
                "workspace root {} is a symlink; refusing live provisioning",
                workspace_root.display()
            );
        }
    }

    let mut path = workspace_root.to_path_buf();
    for component in rel.components() {
        path.push(component.as_os_str());
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if meta.file_type().is_symlink() {
                anyhow::bail!(
                    "workspace path {} contains symlink {}; refusing live provisioning",
                    workspace.display(),
                    path.display()
                );
            }
        }
    }

    Ok(())
}

/// Production provisioner: real `git worktree add` of the ironmem repo at the
/// resolved base commit, no shell. Tests use a fake provisioner; this impl is
/// exercised only behind the approval gate.
pub struct ProcessWorkspaceProvisioner {
    pub ironmem_repo: PathBuf,
}

impl WorkspaceProvisioner for ProcessWorkspaceProvisioner {
    fn provision(&self, req: &ProvisionRequest) -> Result<()> {
        std::fs::create_dir_all(req.workspace_root)
            .with_context(|| format!("creating workspace root {}", req.workspace_root.display()))?;
        ensure_workspace_path_safe(req.workspace_root, req.workspace)?;

        // (1) Create the parent (<root>/<task_id>); git worktree add does not
        // create missing intermediate parents, and only <out_dir>/workspaces is
        // guaranteed to exist by run_task_live_guarded.
        if let Some(parent) = worktree_parent_dir(req.workspace) {
            std::fs::create_dir_all(&parent)
                .with_context(|| format!("creating workspace parent {}", parent.display()))?;
        }
        ensure_workspace_path_safe(req.workspace_root, req.workspace)?;

        if let Some(parent) = worktree_parent_dir(req.workspace) {
            let root = req.workspace_root.canonicalize().with_context(|| {
                format!(
                    "canonicalizing workspace root {}",
                    req.workspace_root.display()
                )
            })?;
            let parent = parent
                .canonicalize()
                .with_context(|| format!("canonicalizing workspace parent {}", parent.display()))?;
            if !parent.starts_with(&root) {
                anyhow::bail!(
                    "workspace parent {} resolves outside workspace root {}",
                    parent.display(),
                    root.display()
                );
            }
        }

        // (2) Stale-worktree guard: never silently reuse a populated leaf. A
        // read_dir error on an EXISTING path (permissions, race) must fail loud
        // with context — not be swallowed as "empty/safe".
        if req.workspace.exists() {
            let non_empty = std::fs::read_dir(req.workspace)
                .with_context(|| {
                    format!(
                        "reading existing workspace {} for stale-worktree check",
                        req.workspace.display()
                    )
                })?
                .next()
                .is_some();
            if non_empty {
                anyhow::bail!(
                    "workspace {} already exists and is non-empty (stale worktree); \
                     refusing to reuse it",
                    req.workspace.display()
                );
            }
        }
        // (3) Validate the base ref exists before adding the worktree (no shell).
        let verify = std::process::Command::new("git")
            .args([
                "-C",
                &self.ironmem_repo.display().to_string(),
                "rev-parse",
                "--verify",
                "--end-of-options",
                &format!("{}^{{commit}}", req.base_commit.as_str()),
            ])
            .output()
            .with_context(|| {
                format!(
                    "verifying base {} in {}",
                    req.base_commit,
                    self.ironmem_repo.display()
                )
            })?;
        if !verify.status.success() {
            anyhow::bail!(
                "task {} base ref {:?} is unknown/ambiguous in repo {}",
                req.task.id,
                req.base_commit.as_str(),
                self.ironmem_repo.display()
            );
        }
        // (4) Add the worktree (no shell, program+argv, same hardening as ProcessGateRunner).
        let (program, argv) =
            worktree_add_argv(&self.ironmem_repo, req.workspace, req.base_commit.as_str());
        let status = std::process::Command::new(&program)
            .args(&argv)
            .status()
            .with_context(|| {
                format!(
                    "git worktree add failed: repo={} base={} workspace={}",
                    self.ironmem_repo.display(),
                    req.base_commit,
                    req.workspace.display()
                )
            })?;
        if !status.success() {
            anyhow::bail!(
                "git worktree add exited non-zero: repo={} base={} workspace={}",
                self.ironmem_repo.display(),
                req.base_commit,
                req.workspace.display()
            );
        }
        Ok(())
    }
}

/// Live executor — builds the arm command, provisions the workspace via the
/// injected [`WorkspaceProvisioner`], runs the command via the injected
/// [`CommandRunner`], and parses the CLI envelope. Spawning a real process is
/// still gated behind the approval guard in `runner::run_task_live_guarded`.
pub struct LiveExecutor<R: CommandRunner, P: WorkspaceProvisioner> {
    runner: R,
    provisioner: P,
    workspace_root: PathBuf,
    base_override: Option<String>,
    ironmem_runner: Box<dyn IronmemArmRunner>,
}

impl<R: CommandRunner, P: WorkspaceProvisioner> LiveExecutor<R, P> {
    pub fn new(
        runner: R,
        provisioner: P,
        workspace_root: PathBuf,
        base_override: Option<String>,
    ) -> Self {
        Self {
            runner,
            provisioner,
            workspace_root,
            base_override,
            ironmem_runner: Box::new(ProcessIronmemArmRunner),
        }
    }

    /// Override the ironmem-arm runner (tests inject a fake so the heavy
    /// real-process collab path is not spawned).
    pub fn with_ironmem_runner(mut self, runner: Box<dyn IronmemArmRunner>) -> Self {
        self.ironmem_runner = runner;
        self
    }

    /// The isolated workspace an arm runs in: `<root>/<task_id>/<arm>`. The
    /// runner uses this to run gates in the same directory the agent produced.
    pub fn workspace_for(&self, task: &Task, arm: Arm) -> PathBuf {
        self.workspace_root.join(&task.id).join(arm.label())
    }
}

impl<R: CommandRunner, P: WorkspaceProvisioner> ArmExecutor for LiveExecutor<R, P> {
    fn execute(&self, task: &Task, arm: Arm) -> Result<ArmOutcome> {
        let workspace = self.workspace_for(task, arm);
        // Single-authority, fail-loud base resolution (see `resolve_base_commit`):
        // exactly one of task pin / run override must be present, else error.
        let base = resolve_base_commit(task, self.base_override.as_deref())?;
        self.provisioner.provision(&ProvisionRequest {
            task,
            arm,
            base_commit: &base,
            workspace_root: &self.workspace_root,
            workspace: &workspace,
        })?;

        // The superpowers arm runs a single `claude -p`; the ironmem arm drives a
        // real /collab flow (Claude + Codex) via the injected IronmemArmRunner.
        if matches!(arm, Arm::Ironmem) {
            // Collab artifacts (collab.db, codex-home, remote.git) live as a
            // SIBLING of the workspaces tree: workspace_root is <out>/workspaces,
            // so its parent <out> is the task-dir root. The unwrap_or fallback only
            // triggers for a root-less workspace_root (not a real run layout).
            let out_task_dir = self
                .workspace_root
                .parent()
                .unwrap_or(&self.workspace_root)
                .join(&task.id);
            std::fs::create_dir_all(&out_task_dir).with_context(|| {
                format!("creating ironmem out dir {}", out_task_dir.display())
            })?;
            return self.ironmem_runner.run(task, &workspace, &out_task_dir);
        }

        let (program, args) = arm_command(task, arm);
        let output = self.runner.run(&program, &args, &workspace)?;
        let parsed = parse_cli_result(&output.stdout)?;

        // A non-zero exit OR an is_error envelope is a non-completion; tokens
        // spent are still recorded so a failed arm is never a silent zero row.
        let completed = output.success && !parsed.is_error;

        // A *successful* run reporting zero total tokens is not physically
        // plausible — it means the CLI `usage` block was absent/renamed (schema
        // drift). Recording it as a merged zero-token row would silently deflate
        // the headline cost metric, so fail loudly rather than measure nothing.
        if completed && parsed.usage.total() == 0 {
            anyhow::bail!(
                "claude reported success for task {} arm {} but the usage block was \
                 absent or zero — refusing to record a zero-token measurement row",
                task.id,
                arm.label()
            );
        }

        let outcome = if completed {
            crate::constants::OUTCOME_COMPLETED
        } else {
            crate::constants::OUTCOME_FAILED
        };

        Ok(ArmOutcome {
            arm,
            usage: parsed.usage,
            codex_usage: Usage::default(),
            review_rounds: 0,
            fix_commits: 0,
            outcome: outcome.to_string(),
            transcript: output.stdout,
        })
    }
}
