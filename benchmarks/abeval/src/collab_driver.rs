//! Headless collab driver: reproduces the interactive `/collab` dispatcher loop
//! (`.claude-plugin/commands/collab.md`) against a live per-task collab session,
//! with two injected seams so the loop is unit-tested with fakes:
//! [`CollabStateReader`] (DB poll) and [`WorkerSpawner`] (claude/codex spawn).

use std::cmp::Reverse;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::client::Usage;
use crate::collab_db::SessionState;

/// Terminal collab phases (mirrors `crates/ironmem/src/collab/phase.rs`).
pub const PHASE_CODING_COMPLETE: &str = "CodingComplete";
pub const PHASE_CODING_FAILED: &str = "CodingFailed";

/// One dispatch decision for a `(phase, owner)` poll. Frozen mirror of the
/// dispatch matrix in `.claude-plugin/commands/collab.md` (owner-first dispatch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAction {
    /// A single Claude turn that sends its phase event directly (no compose/submit
    /// split). `mode` is substituted into `$MODE` (e.g. plan-synthesis revision).
    ClaudeSend { template: &'static str, mode: &'static str },
    /// A Claude compose turn (writes an artifact, returns a `ref:`), then an
    /// auto-approved `collab-turn-submit.md` send of `topic` by that ref.
    ClaudeCompose { template: &'static str, topic: &'static str },
    /// A Codex turn (`codex exec ... join <session>`); usage attributed later.
    Codex,
    /// The final-review compose + a driver-owned synthetic-`pr_url` submit (no
    /// `gh pr create`, nothing pushed).
    FinalReviewSynthetic,
    /// Terminal phase: stop the loop.
    Terminal,
    /// Owner/phase combination that should not occur — stop and surface.
    Anomaly,
}

/// Map a `(phase, owner, global_review_round)` poll to a [`WorkerAction`].
pub fn worker_action(phase: &str, owner: &str, global_review_round: u32) -> WorkerAction {
    if matches!(phase, PHASE_CODING_COMPLETE | PHASE_CODING_FAILED) {
        return WorkerAction::Terminal;
    }
    match owner {
        "codex" => match phase {
            "PlanParallelDrafts" | "PlanCodexReviewPending" | "CodeReviewFixGlobalPending" => {
                WorkerAction::Codex
            }
            _ => WorkerAction::Anomaly,
        },
        "claude" => match phase {
            "PlanParallelDrafts" => WorkerAction::ClaudeSend {
                template: "collab-turn-plan-draft.md",
                mode: "send",
            },
            "PlanSynthesisPending" => {
                if global_review_round == 0 {
                    WorkerAction::ClaudeCompose {
                        template: "collab-turn-plan-synthesis.md",
                        topic: "canonical",
                    }
                } else {
                    WorkerAction::ClaudeSend {
                        template: "collab-turn-plan-synthesis.md",
                        mode: "send",
                    }
                }
            }
            "PlanClaudeFinalizePending" => WorkerAction::ClaudeCompose {
                template: "collab-turn-plan-finalize.md",
                topic: "final",
            },
            "PlanLocked" => WorkerAction::ClaudeCompose {
                template: "collab-turn-task-list.md",
                topic: "task_list",
            },
            "CodeImplementPending" => WorkerAction::ClaudeSend {
                template: "collab-turn-code-implement.md",
                mode: "send",
            },
            "CodeReviewLocalPending" => WorkerAction::ClaudeSend {
                template: "collab-turn-review-local.md",
                mode: "send",
            },
            "CodeReviewFinalPending" => WorkerAction::FinalReviewSynthetic,
            _ => WorkerAction::Anomaly,
        },
        _ => WorkerAction::Anomaly,
    }
}

/// Extract the `ref:` value from a worker's ≤3-line verdict. `none` / absent → None.
pub fn parse_ref_line(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("ref:") {
            let v = rest.trim();
            return if v.is_empty() || v.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(v.to_string())
            };
        }
    }
    None
}

/// Read the `ABEVAL_SESSION_ID=<id>` line the bootstrap worker prints.
pub fn parse_session_id(stdout: &str) -> Result<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("ABEVAL_SESSION_ID=") {
            let id = rest.trim();
            if !id.is_empty() {
                return Ok(id.to_string());
            }
        }
    }
    Err(anyhow!(
        "bootstrap output did not contain an ABEVAL_SESSION_ID=<id> line"
    ))
}

/// Render a worker template by reading `<prompts_dir>/<template>` and replacing
/// each `$VAR` in `subst`. Keys are applied longest-first so that a prefix key
/// (`$ARTIFACT_HASH`) cannot clobber a longer one (`$ARTIFACT_REF`).
pub fn render_worker_prompt(
    prompts_dir: &Path,
    template: &str,
    subst: &[(&str, &str)],
) -> Result<String> {
    let path = prompts_dir.join(template);
    let mut body = std::fs::read_to_string(&path)
        .map_err(|e| anyhow!("reading worker template {}: {e}", path.display()))?;
    let mut keys: Vec<&(&str, &str)> = subst.iter().collect();
    keys.sort_by_key(|(k, _)| Reverse(k.len()));
    for (k, v) in keys {
        body = body.replace(k, v);
    }
    Ok(body)
}

// ── Dispatcher loop ──────────────────────────────────────────────────────────

const MAX_TURNS: usize = 60;

/// Driver-owned synthetic final-review submit (replaces `gh pr create`): the
/// worker sends `final_review` with a synthetic, un-pushed `pr_url`. No network,
/// nothing pushed. `$SESSION_ID` and `$PR_URL` are substituted by the driver.
const SYNTHETIC_FINAL_SUBMIT: &str = "\
You are an abeval headless submit worker. Do NOT run `gh pr create` and do NOT \
push anything. Send the final review event directly:\n\
mcp__ironmem__collab_send with sender=\"claude\", topic=\"final_review\", \
content=<JSON {\"head_sha\":\"<current HEAD of this worktree>\",\"pr_url\":\"$PR_URL\"}> \
for session_id=$SESSION_ID.\n\
Return EXACTLY:\nresult: final_review sent\nref: $PR_URL\nblocker: none\n";

/// Poll the per-task collab session row (DB read seam).
pub trait CollabStateReader {
    fn read(&self, session_id: &str) -> Result<SessionState>;
}

/// Result of one spawned Claude worker turn.
pub struct WorkerResult {
    pub usage: Usage,
    /// The worker's *printed text* — the `--output-format json` envelope's
    /// `result` field, NOT the raw `{...}` wrapper. Sentinel lines parsed by
    /// [`parse_session_id`]/[`parse_ref_line`] live here. See
    /// `collab_live::worker_text_and_usage`.
    pub stdout: String,
}

/// Result of one spawned Codex turn. `commits_added` is the count of commits the
/// turn added to the worktree (used for `fix_commits` on rework turns).
pub struct CodexResult {
    pub commits_added: u32,
}

/// Spawn workers (claude `-p` / `codex exec`). Injected so the loop is tested
/// with a fake; the prod impl lives in `collab_live.rs`.
pub trait WorkerSpawner {
    fn spawn_claude(&self, prompt: &str, worktree: &Path) -> Result<WorkerResult>;
    fn spawn_codex(&self, session_id: &str, worktree: &Path) -> Result<CodexResult>;
}

/// Attribute Codex tokens for this run (rollout scan seam).
pub trait CodexAttributor {
    fn attribute(&self) -> Result<Usage>;
}

/// Per-task inputs to the driver loop.
pub struct CollabTaskCtx {
    pub task_id: String,
    pub worktree: PathBuf,
    pub branch: String,
    pub prompts_dir: PathBuf,
    pub bootstrap_prompt: String,
}

/// Output of one collab run.
#[derive(Debug)]
pub struct CollabRunResult {
    pub claude_usage: Usage,
    pub codex_usage: Usage,
    pub reached_phase: String,
    pub review_rounds: u32,
    pub fix_commits: u32,
    pub pr_url_synthetic: String,
}

/// Run one task through the headless collab dispatcher loop.
pub fn run_collab_task<R: CollabStateReader, S: WorkerSpawner, A: CodexAttributor>(
    ctx: &CollabTaskCtx,
    reader: &R,
    spawner: &S,
    attributor: &A,
) -> Result<CollabRunResult> {
    let wt = ctx.worktree.as_path();
    let synthetic_pr = format!("local://abeval/{}", ctx.task_id);

    let mut claude_usage = Usage::default();
    let mut fix_commits: u32 = 0;

    // (1) Bootstrap: collab_start + print ABEVAL_SESSION_ID=<id>.
    let boot = spawner.spawn_claude(&ctx.bootstrap_prompt, wt)?;
    claude_usage.add_assign(&boot.usage);
    let session_id = parse_session_id(&boot.stdout)?;

    // (2) Dispatcher loop.
    let mut last_state: Option<SessionState> = None;
    for _ in 0..MAX_TURNS {
        let state = reader.read(&session_id)?;
        let action = worker_action(&state.phase, &state.current_owner, state.global_review_round);
        last_state = Some(state.clone());
        match action {
            WorkerAction::Terminal => break,
            WorkerAction::Anomaly => {
                return Err(anyhow!(
                    "collab anomaly: phase {} owned by {} (unexpected)",
                    state.phase,
                    state.current_owner
                ));
            }
            WorkerAction::ClaudeSend { template, mode } => {
                let prompt = render_worker_prompt(
                    &ctx.prompts_dir,
                    template,
                    &[
                        ("$SESSION_ID", &session_id),
                        ("$BRANCH", &ctx.branch),
                        ("$MODE", mode),
                    ],
                )?;
                let r = spawner.spawn_claude(&prompt, wt)?;
                claude_usage.add_assign(&r.usage);
            }
            WorkerAction::ClaudeCompose { template, topic } => {
                let compose = render_worker_prompt(
                    &ctx.prompts_dir,
                    template,
                    &[
                        ("$SESSION_ID", &session_id),
                        ("$BRANCH", &ctx.branch),
                        ("$MODE", "compose"),
                        ("$TOPIC", topic),
                    ],
                )?;
                let cr = spawner.spawn_claude(&compose, wt)?;
                claude_usage.add_assign(&cr.usage);
                let artifact_ref = parse_ref_line(&cr.stdout).ok_or_else(|| {
                    anyhow!("compose worker for {topic} returned no ref: line")
                })?;
                let submit = render_worker_prompt(
                    &ctx.prompts_dir,
                    "collab-turn-submit.md",
                    &[
                        ("$SESSION_ID", &session_id),
                        ("$BRANCH", &ctx.branch),
                        ("$MODE", "submit"),
                        ("$TOPIC", topic),
                        ("$ARTIFACT_REF", &artifact_ref),
                    ],
                )?;
                let sr = spawner.spawn_claude(&submit, wt)?;
                claude_usage.add_assign(&sr.usage);
            }
            WorkerAction::Codex => {
                let cr = spawner.spawn_codex(&session_id, wt)?;
                if state.phase == "CodeReviewFixGlobalPending" {
                    fix_commits = fix_commits.saturating_add(cr.commits_added);
                }
            }
            WorkerAction::FinalReviewSynthetic => {
                let compose = render_worker_prompt(
                    &ctx.prompts_dir,
                    "collab-turn-final-review.md",
                    &[
                        ("$SESSION_ID", &session_id),
                        ("$BRANCH", &ctx.branch),
                        ("$MODE", "compose"),
                    ],
                )?;
                let cr = spawner.spawn_claude(&compose, wt)?;
                claude_usage.add_assign(&cr.usage);
                let submit = SYNTHETIC_FINAL_SUBMIT
                    .replace("$SESSION_ID", &session_id)
                    .replace("$PR_URL", &synthetic_pr);
                let sr = spawner.spawn_claude(&submit, wt)?;
                claude_usage.add_assign(&sr.usage);
            }
        }
    }

    let last_state = last_state.ok_or_else(|| anyhow!("loop never polled a session state"))?;
    let reached_phase = last_state.phase.clone();
    let review_rounds = last_state.global_review_round;

    if !matches!(reached_phase.as_str(), PHASE_CODING_COMPLETE | PHASE_CODING_FAILED) {
        return Err(anyhow!(
            "collab run for task {} exhausted MAX_TURNS ({}) without reaching a \
             terminal phase; last phase was {} — INVALID run",
            ctx.task_id,
            MAX_TURNS,
            reached_phase
        ));
    }

    // (3) Attribute Codex tokens; a completed run with zero Codex is INVALID.
    let codex_usage = attributor.attribute()?;
    if reached_phase == PHASE_CODING_COMPLETE && codex_usage.total() == 0 {
        return Err(anyhow!(
            "collab run for task {} reached CodingComplete but attributed ZERO Codex \
             tokens — INVALID run (no Codex engagement); excluded",
            ctx.task_id
        ));
    }
    if reached_phase == PHASE_CODING_COMPLETE && claude_usage.total() == 0 {
        return Err(anyhow!(
            "collab run for task {} reached CodingComplete but accumulated ZERO Claude \
             tokens across all turns — INVALID run (workers emitted no usage); excluded",
            ctx.task_id
        ));
    }

    Ok(CollabRunResult {
        claude_usage,
        codex_usage,
        reached_phase,
        review_rounds,
        fix_commits,
        pr_url_synthetic: synthetic_pr,
    })
}
