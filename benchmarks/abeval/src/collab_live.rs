//! Production seams for the headless collab driver (real process spawns) + the
//! per-task live environment (isolated CODEX_HOME, local bare remote, branch).
//! The real-process entry point `run_ironmem_arm` is reached ONLY behind the
//! approval gate in `runner::run_task_live_guarded`; the pure argv/config helpers
//! are also used directly by unit tests.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;

use crate::client::{parse_cli_result, ArmOutcome, Usage};
use crate::codex_tokens::{attribute_codex_tokens, TimeWindow};
use crate::collab_db::read_session_state;
use crate::collab_driver::{
    run_collab_task, CodexAttributor, CodexResult, CollabRunResult, CollabStateReader,
    CollabTaskCtx, WorkerResult, WorkerSpawner,
};
use crate::corpus::Task;

/// `claude -p` worker argv: JSON output, headless permissions, and the
/// driver-supplied `--mcp-config` (so the worker's ironmem MCP server shares the
/// per-task DB via inherited `IRONMEM_DB_PATH`). The prompt is appended as the
/// `-p` positional by the caller.
///
/// The trailing `--` is REQUIRED: the collab-turn worker templates begin with
/// `---` YAML frontmatter, so a prompt passed without an end-of-options marker is
/// parsed by the CLI as an option (`error: unknown option '---'`). `--` forces
/// the prompt to be a positional even when it starts with dashes.
pub fn claude_worker_argv(mcp_config: &str) -> (String, Vec<String>) {
    (
        "claude".to_string(),
        vec![
            "--output-format".into(),
            "json".into(),
            "--permission-mode".into(),
            "bypassPermissions".into(),
            "--mcp-config".into(),
            mcp_config.to_string(),
            "-p".into(),
            "--".into(),
        ],
    )
}

/// Extract the worker's *printed text* and token usage from a raw `claude -p
/// --output-format json` envelope. The envelope is a single JSON line, so the
/// model's actual output (where sentinel lines like `ABEVAL_SESSION_ID=` / `ref:`
/// live) is the `result` field — NOT the raw bytes. The driver's line parsers
/// must see that text, not the `{...}` wrapper. On an unparseable envelope
/// (schema drift) fall back to the raw bytes + default usage: a 0-exit worker
/// then contributes zero tokens for the turn (the rare tolerance the run-level
/// zero-token guards still catch in aggregate).
pub fn worker_text_and_usage(raw: &str) -> (String, Usage) {
    match parse_cli_result(raw) {
        Ok(r) => (r.result, r.usage),
        Err(_) => (raw.to_string(), Usage::default()),
    }
}

/// `codex exec` argv: sandbox full-access, run in the worktree, prompt positional.
///
/// The `--` before the prompt is REQUIRED: the collab prompt (`collab.md`) begins
/// with `---` YAML frontmatter, so without an end-of-options marker `codex exec`
/// parses it as a flag (`error: unexpected argument '---'`). Same hazard the
/// Claude worker argv guards against.
pub fn codex_exec_argv(worktree: &Path, prompt: &str) -> (String, Vec<String>) {
    (
        "codex".to_string(),
        vec![
            "exec".into(),
            "-s".into(),
            "danger-full-access".into(),
            "-C".into(),
            worktree.display().to_string(),
            "--".into(),
            prompt.to_string(),
        ],
    )
}

/// `config.toml` for the isolated CODEX_HOME (memory
/// `feedback_codex_app_config_rewrite`): only keys the pinned CLI parses, plus the
/// ironmem MCP server so Codex actually has the `collab_*` tools it needs to take
/// its turn. The server is pinned to THIS task's collab DB (`IRONMEM_DB_PATH`) in
/// trusted mode — the same write-enabled server the Claude workers use — so both
/// agents act on one shared session. Without this block Codex has no collab tools
/// and its turn is a ~2s no-op (`last_agent_message: null`), which the zero-Codex
/// INVALID guard would (correctly) reject.
pub fn codex_config(db_path: &Path) -> String {
    // db_path is harness-controlled (under the out tree), but escape defensively
    // so a `"`/`\` in the path can't produce malformed TOML.
    let db = db_path
        .display()
        .to_string()
        .replace('\\', "\\\\")
        .replace('"', "\\\"");
    // `gpt-5.5` is the model the user's ChatGPT/subscription Codex account
    // supports (and what their real ~/.codex/config.toml uses). `gpt-5-codex` /
    // `gpt-5` return HTTP 400 "not supported when using Codex with a ChatGPT
    // account", which makes every Codex turn a ~2s null no-op.
    format!(
        "model = \"gpt-5.5\"\n\
         model_reasoning_effort = \"xhigh\"\n\
         \n\
         [mcp_servers.ironmem]\n\
         command = \"ironmem\"\n\
         args = [\"serve\"]\n\
         \n\
         [mcp_servers.ironmem.env]\n\
         IRONMEM_DB_PATH = \"{db}\"\n\
         IRONMEM_MCP_MODE = \"trusted\"\n"
    )
}

pub struct SqliteStateReader {
    pub db_path: PathBuf,
}
impl CollabStateReader for SqliteStateReader {
    fn read(&self, session_id: &str) -> Result<crate::collab_db::SessionState> {
        read_session_state(&self.db_path, session_id)
    }

    fn newest_draft_drawer(&self, after_rowid: i64) -> Result<Option<(String, i64)>> {
        crate::collab_db::newest_draft_drawer(&self.db_path, after_rowid)
    }
}

pub struct ProcessWorkerSpawner {
    pub db_path: PathBuf,
    pub codex_home: PathBuf,
    pub mcp_config: String,
}
impl WorkerSpawner for ProcessWorkerSpawner {
    fn spawn_claude(&self, prompt: &str, worktree: &Path) -> Result<WorkerResult> {
        let (prog, mut args) = claude_worker_argv(&self.mcp_config);
        args.push(prompt.to_string());
        let out = std::process::Command::new(&prog)
            .args(&args)
            .current_dir(worktree)
            .env("IRONMEM_DB_PATH", &self.db_path)
            .output()
            .with_context(|| format!("spawning claude worker in {}", worktree.display()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            return Err(anyhow!(
                "claude worker exited {:?} in {} — stderr: {}",
                out.status.code(),
                worktree.display(),
                stderr.trim()
            ));
        }
        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        let (stdout, usage) = worker_text_and_usage(&raw);
        Ok(WorkerResult { usage, stdout })
    }

    fn spawn_codex(&self, session_id: &str, worktree: &Path) -> Result<CodexResult> {
        let head_before = git_head(worktree)?;
        let prompt = codex_collab_prompt(session_id)?;
        let (prog, args) = codex_exec_argv(worktree, &prompt);
        let status = std::process::Command::new(&prog)
            .args(&args)
            .env("CODEX_HOME", &self.codex_home)
            .status()
            .with_context(|| format!("spawning codex exec in {}", worktree.display()))?;
        if !status.success() {
            return Err(anyhow!(
                "codex exec exited non-zero in {}",
                worktree.display()
            ));
        }
        let head_after = git_head(worktree)?;
        let commits_added = count_commits_between(worktree, &head_before, &head_after)?;
        Ok(CodexResult { commits_added })
    }
}

/// Public seam retained for symmetry/external callers; the live path uses the
/// inline `LiveAttributor` in `drive_then_attribute` instead.
#[allow(dead_code)]
pub struct ProcessCodexAttributor {
    pub sessions_root: PathBuf,
    pub worktree: PathBuf,
    pub window: TimeWindow,
}
impl CodexAttributor for ProcessCodexAttributor {
    fn attribute(&self) -> Result<Usage> {
        attribute_codex_tokens(&self.sessions_root, &self.worktree, self.window)
    }
}

fn git_head(worktree: &Path) -> Result<String> {
    let out = std::process::Command::new("git")
        .args(["-C", &worktree.display().to_string(), "rev-parse", "HEAD"])
        .output()
        .with_context(|| format!("git rev-parse HEAD in {}", worktree.display()))?;
    if !out.status.success() {
        return Err(anyhow!(
            "git rev-parse HEAD failed in {}",
            worktree.display()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

fn count_commits_between(worktree: &Path, from: &str, to: &str) -> Result<u32> {
    if from == to {
        return Ok(0);
    }
    let out = std::process::Command::new("git")
        .args([
            "-C",
            &worktree.display().to_string(),
            "rev-list",
            "--count",
            &format!("{from}..{to}"),
        ])
        .output()
        .with_context(|| format!("git rev-list --count in {}", worktree.display()))?;
    if !out.status.success() {
        return Err(anyhow!("git rev-list failed in {}", worktree.display()));
    }
    let raw = String::from_utf8_lossy(&out.stdout);
    raw.trim().parse::<u32>().with_context(|| {
        format!(
            "parsing `git rev-list --count` output {:?} in {}",
            raw.trim(),
            worktree.display()
        )
    })
}

/// Provision the isolated CODEX_HOME (minimal config + symlinked auth.json), a
/// per-task local bare remote wired as `origin`, and the working branch — so the
/// collab workers' intermediate `git push`es succeed locally (nothing reaches the
/// real origin) and the final PR-create is replaced by the synthetic pr_url.
pub fn provision_collab_env(
    worktree: &Path,
    codex_home: &Path,
    auth_src: &Path,
    branch: &str,
    remote_bare: &Path,
    db_path: &Path,
) -> Result<()> {
    // Isolated CODEX_HOME — config carries the ironmem MCP pinned to this task's DB.
    std::fs::create_dir_all(codex_home)
        .with_context(|| format!("creating CODEX_HOME {}", codex_home.display()))?;
    std::fs::write(codex_home.join("config.toml"), codex_config(db_path))?;
    let auth_dst = codex_home.join("auth.json");
    if auth_dst.exists() {
        std::fs::remove_file(&auth_dst)
            .with_context(|| format!("removing stale auth symlink {}", auth_dst.display()))?;
    }
    if !auth_src.exists() {
        return Err(anyhow!(
            "Codex auth not found at {}; cannot provision isolated CODEX_HOME",
            auth_src.display()
        ));
    }
    std::os::unix::fs::symlink(auth_src, &auth_dst)
        .with_context(|| format!("symlinking auth.json into {}", codex_home.display()))?;
    std::fs::create_dir_all(codex_home.join("sessions"))
        .with_context(|| format!("creating sessions dir under {}", codex_home.display()))?;

    // Per-task local bare remote so intermediate pushes go nowhere real.
    run_git(&["init", "--bare", &remote_bare.display().to_string()])?;
    run_git_in(worktree, &["checkout", "-b", branch])?;
    // The isolated clone already carries an `origin` (→ the source repo); repoint
    // it at this task's throwaway bare so collab workers' `git push origin` stays
    // local. Because the workspace is a clone (not a linked worktree) this mutates
    // only the per-task config — never the real repo's. Fall back to `add` if the
    // clone somehow lacks an origin.
    wire_origin(worktree, remote_bare)?;
    run_git_in(worktree, &["push", "-u", "origin", branch])?;
    Ok(())
}

/// Point the workspace's `origin` at `bare`, whether or not an `origin` already
/// exists. A clone has one (→ source repo) so `set-url` is the normal path; the
/// `add` fallback covers a repo that lacks it.
fn wire_origin(worktree: &Path, bare: &Path) -> Result<()> {
    let bare_s = bare.display().to_string();
    if run_git_in(worktree, &["remote", "set-url", "origin", &bare_s]).is_err() {
        run_git_in(worktree, &["remote", "add", "origin", &bare_s])?;
    }
    Ok(())
}

fn run_git(args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("git")
        .args(args)
        .status()
        .with_context(|| format!("git {args:?}"))?;
    if !status.success() {
        return Err(anyhow!("git {args:?} failed"));
    }
    Ok(())
}
fn run_git_in(dir: &Path, args: &[&str]) -> Result<()> {
    let status = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .with_context(|| format!("git -C {} {args:?}", dir.display()))?;
    if !status.success() {
        return Err(anyhow!("git -C {} {args:?} failed", dir.display()));
    }
    Ok(())
}

/// Drive the ironmem arm for one task: provision the live env, run the collab
/// loop, and map the result into an `ArmOutcome`. Reached only behind the
/// approval gate. `worktree` is the already-provisioned detached worktree at the
/// task's base commit (`client::ProcessWorkspaceProvisioner` created it).
pub fn run_ironmem_arm(task: &Task, worktree: &Path, out_task_dir: &Path) -> Result<ArmOutcome> {
    let db_path = out_task_dir.join("collab.db");
    let codex_home = out_task_dir.join("codex-home");
    let remote_bare = out_task_dir.join("remote.git");
    let branch = format!("abeval/{}", task.id);
    let home = std::env::var("HOME").context("HOME unset")?;
    let auth_src = PathBuf::from(&home).join(".codex").join("auth.json");
    let sessions_root = codex_home.join("sessions");

    provision_collab_env(
        worktree,
        &codex_home,
        &auth_src,
        &branch,
        &remote_bare,
        &db_path,
    )?;

    // MCP config: a single ironmem server that inherits IRONMEM_DB_PATH so the
    // worker's collab tools and the driver's reader share the per-task DB. Built
    // with serde_json so a db_path containing `"`/`\` cannot produce malformed JSON.
    //
    // IRONMEM_MCP_MODE=trusted is REQUIRED: the server defaults to read-only, which
    // exposes only the read-class collab tools (status/recv/get_caps/wait_my_turn)
    // and disables the write-class ones the driver depends on — collab_start,
    // collab_send, collab_ack, collab_approve, collab_end. Without it the bootstrap
    // worker cannot create a session and the run fails with "No such tool available:
    // mcp__ironmem__collab_start".
    let mcp_config = serde_json::json!({
        "mcpServers": {
            "ironmem": {
                "command": "ironmem",
                "args": ["serve"],
                "env": {
                    "IRONMEM_DB_PATH": db_path.display().to_string(),
                    "IRONMEM_MCP_MODE": "trusted"
                }
            }
        }
    })
    .to_string();
    let bootstrap_prompt = format!(
        "ABEVAL_BOOTSTRAP: call mcp__ironmem__collab_start with repo_path=\"{}\", \
         branch=\"{}\", task=\"{}\", initiator=\"claude\". Then print EXACTLY one line: \
         ABEVAL_SESSION_ID=<the returned session_id> and nothing after it.",
        worktree.display(),
        branch,
        task.prompt.replace('"', "'"),
    );

    let window_start = Utc::now();
    let ctx = CollabTaskCtx {
        task_id: task.id.clone(),
        worktree: worktree.to_path_buf(),
        branch,
        prompts_dir: prompts_dir()?,
        bootstrap_prompt,
    };
    let reader = SqliteStateReader {
        db_path: db_path.clone(),
    };
    let spawner = ProcessWorkerSpawner {
        db_path: db_path.clone(),
        codex_home: codex_home.clone(),
        mcp_config,
    };
    let result = drive_then_attribute(&ctx, &reader, &spawner, &sessions_root, window_start)?;

    Ok(ArmOutcome {
        arm: crate::arms::Arm::Ironmem,
        usage: result.claude_usage,
        codex_usage: result.codex_usage,
        review_rounds: result.review_rounds,
        fix_commits: result.fix_commits,
        outcome: if result.reached_phase == crate::collab_driver::PHASE_CODING_COMPLETE {
            crate::constants::OUTCOME_COMPLETED.to_string()
        } else {
            crate::constants::OUTCOME_FAILED.to_string()
        },
        transcript: format!("collab reached {}", result.reached_phase),
    })
}

/// Run the loop with a deferred attributor: the time window's end is `Utc::now()`
/// captured AFTER the loop, so every Codex rollout written during the run is in
/// range.
fn drive_then_attribute(
    ctx: &CollabTaskCtx,
    reader: &SqliteStateReader,
    spawner: &ProcessWorkerSpawner,
    sessions_root: &Path,
    window_start: chrono::DateTime<Utc>,
) -> Result<CollabRunResult> {
    struct LiveAttributor<'a> {
        sessions_root: &'a Path,
        worktree: &'a Path,
        start: chrono::DateTime<Utc>,
    }
    impl CodexAttributor for LiveAttributor<'_> {
        fn attribute(&self) -> Result<Usage> {
            let window = TimeWindow {
                start: self.start,
                end: Utc::now(),
            };
            attribute_codex_tokens(self.sessions_root, self.worktree, window)
        }
    }
    let attributor = LiveAttributor {
        sessions_root,
        worktree: &ctx.worktree,
        start: window_start,
    };
    run_collab_task(ctx, reader, spawner, &attributor)
}

/// Repo `.claude-plugin/prompts` dir (two parents up from the crate manifest).
fn prompts_dir() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(".claude-plugin").join("prompts"))
        .ok_or_else(|| anyhow!("cannot derive repo .claude-plugin/prompts dir"))
}

/// Repo `.codex-plugin/prompts` dir — the Codex-side collab prompt lives here
/// (the counterpart to `.claude-plugin/prompts`).
fn codex_prompts_dir() -> Result<PathBuf> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(".codex-plugin").join("prompts"))
        .ok_or_else(|| anyhow!("cannot derive repo .codex-plugin/prompts dir"))
}

/// The Codex turn prompt: the full IronRace collab protocol prompt
/// (`.codex-plugin/prompts/collab.md`) with its `$ARGUMENTS` placeholder bound to
/// `join <session_id>`. The prompt itself branches by reading `collab_status`, so
/// one prompt serves every Codex-owned phase (plan draft/review, global review).
/// This replaces the bare `join <sid>` stub that left Codex with no instructions.
pub fn codex_collab_prompt(session_id: &str) -> Result<String> {
    let path = codex_prompts_dir()?.join("collab.md");
    let body = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("reading codex collab prompt {}: {e}", path.display()))?;
    Ok(body.replace("$ARGUMENTS", &format!("join {session_id}")))
}
