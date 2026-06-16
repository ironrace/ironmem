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
use crate::corpus::Task;

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
    pub usage: Usage,
    /// `"completed"` for dry-run synthesis; live outcomes are recorded by the
    /// future live path / normalized metric input.
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
    vec!["--permission-mode".to_string(), "bypassPermissions".to_string()]
}

/// Build the command for an arm. Returns the program and args that are spawned
/// by a [`CommandRunner`].
///
/// - `ironmem` arm: starts a `/collab` flow for the task (carried INSIDE the
///   print-mode `-p` prompt string so the command is genuinely non-interactive
///   and JSON-emitting).
/// - `superpowers` arm: runs the task prompt with superpowers skills ONLY
///   (C1: NO `/collab`, NO semantic search/KG/drawer writes, NO ironmem
///   server-side state in the working context). Any task_tag/reporting
///   instrumentation is measurement-only and kept out of the working path.
///
/// Both arms request `--output-format json` AND `-p` (print mode, required for
/// `--output-format` to take effect) AND headless permission tokens.
pub fn arm_command(task: &Task, arm: Arm) -> (String, Vec<String>) {
    let mut argv = vec!["--output-format".to_string(), "json".to_string()];
    argv.extend(headless_permission_args());
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

/// Live executor — builds the arm command, runs it via the injected
/// [`CommandRunner`] in a per-task/arm workspace, and parses the CLI envelope
/// into an [`ArmOutcome`]. Spawning a real process is still gated behind the
/// approval guard in `runner::run_task_live_guarded`.
pub struct LiveExecutor<R: CommandRunner> {
    runner: R,
    workspace_root: PathBuf,
}

impl<R: CommandRunner> LiveExecutor<R> {
    pub fn new(runner: R, workspace_root: PathBuf) -> Self {
        Self {
            runner,
            workspace_root,
        }
    }

    /// The isolated workspace an arm runs in: `<root>/<task_id>/<arm>`. The
    /// runner uses this to run gates in the same directory the agent produced.
    pub fn workspace_for(&self, task: &Task, arm: Arm) -> PathBuf {
        self.workspace_root.join(&task.id).join(arm.label())
    }
}

impl<R: CommandRunner> ArmExecutor for LiveExecutor<R> {
    fn execute(&self, task: &Task, arm: Arm) -> Result<ArmOutcome> {
        let (program, args) = arm_command(task, arm);
        let workspace = self.workspace_for(task, arm);
        std::fs::create_dir_all(&workspace)
            .with_context(|| format!("creating workspace {}", workspace.display()))?;

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
            outcome: outcome.to_string(),
            transcript: output.stdout,
        })
    }
}
