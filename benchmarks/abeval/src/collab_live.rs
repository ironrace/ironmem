//! Production seams for the headless collab driver (real process spawns) + the
//! per-task live environment (isolated CODEX_HOME, local bare remote, branch).
//! The real-process entry point `run_ironmem_arm` is reached ONLY behind the
//! approval gate in `runner::run_task_live_guarded`; the pure argv/config helpers
//! are also used directly by unit tests.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;

use crate::client::{ArmOutcome, Usage};
use crate::codex_tokens::{attribute_codex_tokens, TimeWindow};
use crate::collab_db::read_session_state;
use crate::collab_driver::{
    run_collab_task, CodexAttributor, CodexResult, CollabRunResult, CollabStateReader,
    CollabTaskCtx, ModelTier, WorkerResult, WorkerSpawner,
};
use crate::corpus::Task;
use crate::proc_timeout::{run_with_timeout, turn_timeout};
use crate::stream_usage::parse_stream_json;

/// `claude -p` worker argv: `stream-json` output (with the required `--verbose`),
/// headless permissions, and the driver-supplied `--mcp-config` (so the worker's
/// ironmem MCP server shares the per-task DB via inherited `IRONMEM_DB_PATH`). The
/// prompt is appended as the `-p` positional by the caller.
///
/// `stream-json` (over the single-envelope `json`) is what lets
/// [`worker_text_and_usage`] sum subagent token usage: a collab implement/review
/// turn fans out to Task-subagents whose tokens never appear in the single
/// envelope's top-level `usage` (METRICS_SPEC §12 2026-06-19).
///
/// The trailing `--` is REQUIRED: the collab-turn worker templates begin with
/// `---` YAML frontmatter, so a prompt passed without an end-of-options marker is
/// parsed by the CLI as an option (`error: unknown option '---'`). `--` forces
/// the prompt to be a positional even when it starts with dashes.
///
/// `--model <tier>` pins the per-turn model: the turn-template `model:` frontmatter
/// is inert under `claude -p`, so the headless driver pins the tier here (memory
/// `project_abeval_campaign_model_tiering`). The flag precedes the trailing `-p --`
/// so the model value is never swallowed as the prompt positional.
pub fn claude_worker_argv(mcp_config: &str, model: ModelTier) -> (String, Vec<String>) {
    (
        "claude".to_string(),
        vec![
            "--output-format".into(),
            "stream-json".into(),
            "--verbose".into(),
            "--permission-mode".into(),
            "bypassPermissions".into(),
            "--model".into(),
            model.as_flag().into(),
            "--mcp-config".into(),
            mcp_config.to_string(),
            "-p".into(),
            "--".into(),
        ],
    )
}

/// Max bytes of a worker's stdout tail included in a non-zero-exit error. The
/// terminal `result`/synthetic-error event (the actionable cause) lives at the
/// END of the transcript, so we keep the tail and drop the head.
const WORKER_STDOUT_TAIL_BYTES: usize = 2048;

/// Build the error message for a worker process that exited non-zero. For
/// `claude -p` the actionable cause (the terminal `result` event / synthetic
/// error such as a session-limit notice) is printed to STDOUT, not stderr, so a
/// bounded tail of stdout is surfaced alongside the exit code and stderr. Pure
/// (no spawn) so the formatting is unit-tested directly.
pub fn format_worker_failure(
    label: &str,
    code: Option<i32>,
    location: &str,
    stderr: &str,
    stdout: &str,
) -> String {
    let tail = stdout_tail(stdout.trim(), WORKER_STDOUT_TAIL_BYTES);
    format!(
        "{label} exited {code:?} in {location} — stderr: {} — stdout tail: {}",
        stderr.trim(),
        tail
    )
}

/// Keep at most `max` bytes from the END of `s`, on a UTF-8 char boundary, with
/// a leading ellipsis when the head was dropped.
fn stdout_tail(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut start = s.len() - max;
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    format!("…{}", &s[start..])
}

/// Outcome of extracting a worker turn's printed text + usage from its raw
/// transcript. `usage_unparseable` records that `usage` is a fallback ZERO rather
/// than a measured value — the driver propagates it so a completed run with any
/// unparseable turn is excluded (its Claude `tokens_to_done` is undercounted).
pub struct WorkerText {
    pub text: String,
    pub usage: Usage,
    pub usage_unparseable: bool,
    pub is_error: bool,
}

/// Extract the worker's *printed text* and token usage from a raw `claude -p
/// --output-format stream-json --verbose` transcript. The model's actual output
/// (where sentinel lines like `ABEVAL_SESSION_ID=` / `ref:` live) is the terminal
/// `result` event's `result` field — NOT the raw JSONL bytes. The driver's line
/// parsers must see that text, not the event wrappers. The returned usage is
/// summed across every assistant message (parent + subagents), so subagent tokens
/// are counted. On an unparseable transcript (schema drift / no `result` event)
/// fall back to the raw bytes + default usage AND set `usage_unparseable`: the
/// sentinel-text path still gets the raw bytes, but the zero usage is flagged
/// loud (here) and surfaced to the run-level guard (a single drifted turn would
/// otherwise undercount silently — the all-zero guard can't see a partial loss).
pub fn worker_text_and_usage(raw: &str) -> WorkerText {
    match parse_stream_json(raw) {
        Ok(r) => WorkerText {
            text: r.result,
            usage: r.usage,
            usage_unparseable: false,
            is_error: r.is_error,
        },
        Err(e) => {
            eprintln!(
                "abeval: claude worker transcript was unparseable, recording ZERO \
                 tokens for this turn (undercount risk): {e}"
            );
            WorkerText {
                text: raw.to_string(),
                usage: Usage::default(),
                usage_unparseable: true,
                is_error: false,
            }
        }
    }
}

/// `codex exec` argv: sandbox full-access, run in the worktree, prompt positional.
///
/// The `--` before the prompt is REQUIRED: the rendered collab prompt begins
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

pub fn collab_outcome_for(
    disposition: crate::collab_driver::RunDisposition,
    reached_phase: &str,
) -> &'static str {
    match disposition {
        crate::collab_driver::RunDisposition::Terminal => {
            if reached_phase == crate::collab_driver::PHASE_CODING_COMPLETE {
                crate::constants::OUTCOME_COMPLETED
            } else {
                crate::constants::OUTCOME_FAILED
            }
        }
        crate::collab_driver::RunDisposition::ExcludedRetryable => {
            crate::constants::OUTCOME_EXCLUDED
        }
    }
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
    fn spawn_claude(
        &self,
        prompt: &str,
        worktree: &Path,
        model: ModelTier,
    ) -> Result<WorkerResult> {
        let (prog, mut args) = claude_worker_argv(&self.mcp_config, model);
        args.push(prompt.to_string());
        let mut cmd = std::process::Command::new(&prog);
        cmd.args(&args)
            .current_dir(worktree)
            .env("IRONMEM_DB_PATH", &self.db_path);
        // Bound the turn: a hung worker must not stall the driver indefinitely,
        // and a timeout kill reaps the per-turn `ironmem serve` MCP child too.
        let out = run_with_timeout(cmd, turn_timeout())
            .with_context(|| format!("claude worker in {}", worktree.display()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            return Err(anyhow!(format_worker_failure(
                "claude worker",
                out.status.code(),
                &worktree.display().to_string(),
                &stderr,
                &stdout,
            )));
        }
        let raw = String::from_utf8_lossy(&out.stdout).into_owned();
        let parsed = worker_text_and_usage(&raw);
        Ok(WorkerResult {
            usage: parsed.usage,
            stdout: parsed.text,
            usage_unparseable: parsed.usage_unparseable,
            is_error: parsed.is_error,
        })
    }

    fn spawn_codex(&self, session_id: &str, worktree: &Path) -> Result<CodexResult> {
        let head_before = git_head(worktree)?;
        let prompt = codex_collab_prompt(session_id)?;
        let (prog, args) = codex_exec_argv(worktree, &prompt);
        let mut cmd = std::process::Command::new(&prog);
        cmd.args(&args).env("CODEX_HOME", &self.codex_home);
        // Same per-turn watchdog as the Claude worker (Codex review/fix turns can
        // also hang); output is captured rather than streamed — the durable record
        // is the rollout jsonl, which `attribute_codex_tokens` reads.
        let out = run_with_timeout(cmd, turn_timeout())
            .with_context(|| format!("codex exec in {}", worktree.display()))?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let stdout = String::from_utf8_lossy(&out.stdout);
            return Err(anyhow!(format_worker_failure(
                "codex exec",
                out.status.code(),
                &worktree.display().to_string(),
                &stderr,
                &stdout,
            )));
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
        // Map the run disposition (not just the phase) to the outcome string: a
        // worker-abort run never reached a terminal phase, and an external
        // session/rate-limit abort must be EXCLUDED, never recorded as FAILED.
        outcome: collab_outcome_for(result.disposition, &result.reached_phase).to_string(),
        transcript: format!(
            "collab reached {} ({:?})",
            result.reached_phase, result.disposition
        ),
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

/// The Codex turn prompt: the interactive collab shim with its `$ARGUMENTS`
/// placeholder bound to `join <session_id>`. The shim reads `collab_status` and
/// selects the installed phase prompt, so one entrypoint serves every Codex-owned
/// phase without relying on the removed monolithic prompt.
pub fn codex_collab_prompt(session_id: &str) -> Result<String> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let path = manifest
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join(".codex-plugin").join("commands").join("collab.md"))
        .ok_or_else(|| anyhow!("cannot derive repo .codex-plugin/commands dir"))?;
    let body = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("reading codex collab prompt {}: {e}", path.display()))?;
    Ok(body.replace("$ARGUMENTS", &format!("join {session_id}")))
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    use anyhow::{bail, Context, Result};

    use super::ProcessWorkerSpawner;
    use crate::collab_driver::{ModelTier, WorkerSpawner};

    static ENV_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

    #[test]
    fn spawn_claude_nonzero_surfaces_stdout_tail() -> Result<()> {
        let bin_dir = tempfile::tempdir()?;
        write_fake_worker(
            bin_dir.path(),
            "claude",
            "CLAUDE_STDOUT_CAUSE",
            "claude stderr",
        )?;

        let msg = with_fake_worker_on_path(bin_dir.path(), || {
            let worktree = tempfile::tempdir()?;
            let spawner = test_spawner();
            match spawner.spawn_claude("prompt", worktree.path(), ModelTier::Opus) {
                Ok(_) => bail!("fake claude unexpectedly succeeded"),
                Err(err) => Ok(err.to_string()),
            }
        })?;

        assert!(
            msg.contains("CLAUDE_STDOUT_CAUSE"),
            "actual spawn_claude error must include stdout tail: {msg}"
        );
        assert!(
            msg.contains("claude stderr"),
            "actual spawn_claude error must retain stderr: {msg}"
        );
        Ok(())
    }

    #[test]
    fn spawn_codex_nonzero_surfaces_stdout_tail() -> Result<()> {
        let bin_dir = tempfile::tempdir()?;
        write_fake_worker(
            bin_dir.path(),
            "codex",
            "CODEX_STDOUT_CAUSE",
            "codex stderr",
        )?;

        let msg = with_fake_worker_on_path(bin_dir.path(), || {
            let worktree = tempfile::tempdir()?;
            init_git_repo(worktree.path())?;
            let spawner = test_spawner();
            match spawner.spawn_codex("session-1", worktree.path()) {
                Ok(_) => bail!("fake codex unexpectedly succeeded"),
                Err(err) => Ok(err.to_string()),
            }
        })?;

        assert!(
            msg.contains("CODEX_STDOUT_CAUSE"),
            "actual spawn_codex error must include stdout tail: {msg}"
        );
        assert!(
            msg.contains("codex stderr"),
            "actual spawn_codex error must retain stderr: {msg}"
        );
        Ok(())
    }

    fn test_spawner() -> ProcessWorkerSpawner {
        ProcessWorkerSpawner {
            db_path: "/tmp/abeval-test.db".into(),
            codex_home: "/tmp/abeval-codex-home".into(),
            mcp_config: "{}".to_string(),
        }
    }

    fn write_fake_worker(
        bin_dir: &Path,
        name: &str,
        stdout_marker: &str,
        stderr_marker: &str,
    ) -> Result<()> {
        let path = bin_dir.join(name);
        std::fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '{stdout_marker}'\nprintf '%s\\n' '{stderr_marker}' >&2\nexit 7\n"
            ),
        )
        .with_context(|| format!("writing fake worker {}", path.display()))?;

        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms)?;
        Ok(())
    }

    fn with_fake_worker_on_path<T>(bin_dir: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
        let _guard = ENV_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .expect("test env lock poisoned");
        let old_path = std::env::var_os("PATH");
        let mut paths = vec![bin_dir.to_path_buf()];
        if let Some(old) = old_path.as_ref() {
            paths.extend(std::env::split_paths(old));
        }
        let joined = std::env::join_paths(paths).context("building test PATH")?;
        std::env::set_var("PATH", &joined);

        let result = f();

        match old_path {
            Some(path) => std::env::set_var("PATH", path),
            None => std::env::remove_var("PATH"),
        }
        result
    }

    fn init_git_repo(worktree: &Path) -> Result<()> {
        run_git(worktree, &["init"])?;
        run_git(
            worktree,
            &["config", "user.email", "abeval@example.invalid"],
        )?;
        run_git(worktree, &["config", "user.name", "abeval test"])?;
        run_git(worktree, &["config", "commit.gpgsign", "false"])?;
        std::fs::write(worktree.join("README.md"), "test repo\n")?;
        run_git(worktree, &["add", "README.md"])?;
        run_git(worktree, &["commit", "-m", "init"])?;
        Ok(())
    }

    fn run_git(worktree: &Path, args: &[&str]) -> Result<()> {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(worktree)
            .args(args)
            .output()
            .with_context(|| format!("git -C {} {args:?}", worktree.display()))?;
        if !out.status.success() {
            bail!(
                "git -C {} {args:?} failed: {}",
                worktree.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        Ok(())
    }
}
