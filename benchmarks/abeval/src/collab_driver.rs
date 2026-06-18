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
    /// A single Claude turn that sends its phase event directly. `mode` is
    /// substituted into `$MODE` for templates that still branch.
    ClaudeSend {
        template: &'static str,
        mode: &'static str,
    },
    /// A Claude compose turn (writes an artifact, returns a `ref:`), then an
    /// auto-approved `collab-turn-submit.md` send of `topic` by that ref.
    ClaudeCompose {
        template: &'static str,
        topic: &'static str,
    },
    /// The PlanLocked v3 bridge: one mechanical worker parses the approved final
    /// Superpowers markdown and sends `task_list`. The old two-step bridge
    /// was removed so no second planning step runs after the final human gate.
    TaskListBridge,
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

/// Map a `(phase, owner)` poll to a [`WorkerAction`].
pub fn worker_action(phase: &str, owner: &str, _review_round: u32) -> WorkerAction {
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
            "PlanSynthesisPending" => WorkerAction::ClaudeSend {
                template: "collab-turn-plan-synthesis.md",
                mode: "send",
            },
            "PlanClaudeFinalizePending" => WorkerAction::ClaudeCompose {
                template: "collab-turn-plan-finalize.md",
                topic: "final",
            },
            "PlanLocked" => WorkerAction::TaskListBridge,
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

/// Extract a non-empty blocker from a worker verdict. `none` / absent → None.
pub fn parse_blocker_line(stdout: &str) -> Option<String> {
    for line in stdout.lines() {
        if let Some(rest) = line.trim().strip_prefix("blocker:") {
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

/// Consecutive worker turns dispatched against an unchanged `(phase, owner,
/// global_review_round)` before the run is declared stalled. A productive turn
/// always changes at least one of those (owner flip within a phase, phase
/// advance, or review-round bump), so repeats mean the turn returns without
/// advancing the session — a hung/looping turn or a submit that never lands.
/// Bailing here bounds wasted work to ~`STUCK_LIMIT` turns instead of grinding
/// the full `MAX_TURNS`.
const STUCK_LIMIT: usize = 2;

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

    /// Newest `collab-drafts` drawer with rowid strictly greater than
    /// `after_rowid`, as `(drawer_id, rowid)`; `after_rowid = i64::MIN` returns
    /// the newest overall. `None` when none exists. Used to recover a compose
    /// worker's artifact ref from the drawer it actually persisted rather than
    /// trusting it to echo a `ref:` line.
    fn newest_draft_drawer(&self, after_rowid: i64) -> Result<Option<(String, i64)>>;
}

/// Resolve the artifact ref produced by a compose worker, robust to the worker
/// omitting or fabricating its `ref:` line (drawer-staging flakiness).
///
/// `before_rowid` is the newest `collab-drafts` drawer rowid captured *before*
/// the compose turn ran. Resolution order:
/// 1. The drawer the worker actually persisted this turn (rowid advanced past
///    `before_rowid`) — authoritative, independent of stdout.
/// 2. The worker's printed `ref:` line — covers topics that stage to a file
///    rather than a drawer (e.g. `task_list`'s `plan_file_path`).
fn resolve_compose_ref<R: CollabStateReader>(
    reader: &R,
    stdout: &str,
    before_rowid: i64,
    topic: &str,
) -> Result<String> {
    if let Some((id, _)) = reader.newest_draft_drawer(before_rowid)? {
        return Ok(id);
    }
    if let Some(id) = parse_ref_line(stdout) {
        return Ok(id);
    }
    Err(anyhow!(
        "compose worker for {topic} persisted no new collab-drafts drawer \
         and printed no ref: line"
    ))
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
    // Stall guard: the `(phase, owner, plan_round, global_round)` key last
    // dispatched, and how many
    // consecutive turns have left it unchanged.
    let mut stall_key: Option<(String, String, u32, u32)> = None;
    let mut stall_count: usize = 0;
    for _ in 0..MAX_TURNS {
        let state = reader.read(&session_id)?;
        let key = (
            state.phase.clone(),
            state.current_owner.clone(),
            state.review_round,
            state.global_review_round,
        );
        if stall_key.as_ref() == Some(&key) {
            stall_count += 1;
            if stall_count >= STUCK_LIMIT {
                return Err(anyhow!(
                    "collab run for task {} stalled: phase {} (owner {}, plan round {}, global round {}) did not \
                     advance after {} consecutive worker turns — INVALID run (hung or \
                     looping turn / submit never landed)",
                    ctx.task_id,
                    state.phase,
                    state.current_owner,
                    state.review_round,
                    state.global_review_round,
                    STUCK_LIMIT
                ));
            }
        } else {
            stall_count = 0;
            stall_key = Some(key);
        }
        let action = worker_action(&state.phase, &state.current_owner, state.review_round);
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
                // Snapshot the newest staged drawer before the turn so we can
                // identify the one this compose persists (rowid advances).
                let before_rowid = reader
                    .newest_draft_drawer(i64::MIN)?
                    .map(|(_, rowid)| rowid)
                    .unwrap_or(i64::MIN);
                let cr = spawner.spawn_claude(&compose, wt)?;
                claude_usage.add_assign(&cr.usage);
                let artifact_ref = resolve_compose_ref(reader, &cr.stdout, before_rowid, topic)?;
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
            WorkerAction::TaskListBridge => {
                let prompt = render_worker_prompt(
                    &ctx.prompts_dir,
                    "collab-turn-task-list.md",
                    &[("$SESSION_ID", &session_id), ("$BRANCH", &ctx.branch)],
                )?;
                let r = spawner.spawn_claude(&prompt, wt)?;
                claude_usage.add_assign(&r.usage);
                if let Some(blocker) = parse_blocker_line(&r.stdout) {
                    return Err(anyhow!(
                        "task_list bridge for task {} returned blocker: {} — INVALID run",
                        ctx.task_id,
                        blocker
                    ));
                }
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

    if !matches!(
        reached_phase.as_str(),
        PHASE_CODING_COMPLETE | PHASE_CODING_FAILED
    ) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal reader fake: `read` is unused here; only the drawer seam matters.
    struct DrawerReader(Option<(String, i64)>);
    impl CollabStateReader for DrawerReader {
        fn read(&self, _session_id: &str) -> Result<SessionState> {
            unreachable!("resolve_compose_ref does not poll session state")
        }
        fn newest_draft_drawer(&self, after_rowid: i64) -> Result<Option<(String, i64)>> {
            Ok(self.0.clone().filter(|(_, rowid)| *rowid > after_rowid))
        }
    }

    #[test]
    fn persisted_drawer_is_authoritative_over_stdout_ref() {
        // A new drawer persisted (rowid 7 > snapshot 3) wins even when the worker
        // also printed a (different) ref: line.
        let reader = DrawerReader(Some(("db-drawer".into(), 7)));
        let got = resolve_compose_ref(&reader, "ref: stdout-drawer\n", 3, "canonical").unwrap();
        assert_eq!(got, "db-drawer");
    }

    #[test]
    fn falls_back_to_stdout_ref_when_no_new_drawer() {
        // No drawer advanced past the snapshot (rowid 3 not > 3): use the printed
        // ref: line. Covers file-staged topics (e.g. task_list's plan_file_path).
        let reader = DrawerReader(Some(("stale".into(), 3)));
        let got = resolve_compose_ref(&reader, "ref: docs/plans/x.md\n", 3, "task_list").unwrap();
        assert_eq!(got, "docs/plans/x.md");
    }

    #[test]
    fn errors_naming_ref_when_neither_drawer_nor_ref_present() {
        let reader = DrawerReader(None);
        let err = resolve_compose_ref(&reader, "result: ok\nblocker: none\n", i64::MIN, "final")
            .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("ref"),
            "error must name 'ref': {err}"
        );
    }

    /// A reader pinned to one never-advancing state, plus minimal spawner/
    /// attributor fakes, to exercise the stall guard without touching the
    /// filesystem. The pinned phase is Codex-owned so the dispatched action is
    /// [`WorkerAction::Codex`] (no worker-template file read).
    struct StuckReader(SessionState);
    impl CollabStateReader for StuckReader {
        fn read(&self, _session_id: &str) -> Result<SessionState> {
            Ok(self.0.clone())
        }
        fn newest_draft_drawer(&self, _after_rowid: i64) -> Result<Option<(String, i64)>> {
            Ok(None)
        }
    }

    struct FakeSpawner;
    impl WorkerSpawner for FakeSpawner {
        fn spawn_claude(&self, _prompt: &str, _worktree: &Path) -> Result<WorkerResult> {
            // Only the bootstrap turn hits this path here; emit the session sentinel.
            Ok(WorkerResult {
                usage: Usage::default(),
                stdout: "ABEVAL_SESSION_ID=s-stuck\n".to_string(),
            })
        }
        fn spawn_codex(&self, _session_id: &str, _worktree: &Path) -> Result<CodexResult> {
            Ok(CodexResult { commits_added: 0 })
        }
    }

    struct FakeAttributor;
    impl CodexAttributor for FakeAttributor {
        fn attribute(&self) -> Result<Usage> {
            unreachable!("stall bail returns before Codex attribution")
        }
    }

    fn pinned_state(phase: &str, owner: &str) -> SessionState {
        SessionState {
            phase: phase.to_string(),
            current_owner: owner.to_string(),
            implementer: "claude".to_string(),
            pr_url: None,
            global_review_round: 0,
            review_round: 0,
            task_review_round: 0,
            last_head_sha: None,
        }
    }

    #[test]
    fn stalled_phase_bails_invalid_before_exhausting_max_turns() {
        let reader = StuckReader(pinned_state("CodeReviewFixGlobalPending", "codex"));
        let ctx = CollabTaskCtx {
            task_id: "abeval-test".into(),
            worktree: PathBuf::from("/tmp/nonexistent-wt"),
            branch: "abeval/test".into(),
            prompts_dir: PathBuf::from("/tmp/nonexistent-prompts"),
            bootstrap_prompt: "boot".into(),
        };
        let err = run_collab_task(&ctx, &reader, &FakeSpawner, &FakeAttributor).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("stalled"), "must report a stall: {msg}");
        assert!(msg.contains("INVALID"), "must mark INVALID: {msg}");
        assert!(
            msg.contains("CodeReviewFixGlobalPending"),
            "must name the stuck phase: {msg}"
        );
    }
}
