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

/// Which Claude model a worker turn pins via `--model`. The interactive
/// orchestrator honors the turn-template `model:` frontmatter through its
/// `Agent(model=)` calls, but that frontmatter is INERT under `claude -p`, so the
/// headless driver must pin the tier on the argv itself. Locked tiering (memory
/// `project_abeval_campaign_model_tiering`): planning + review run on opus (deepest
/// reasoning for design/review); mechanical and implementation turns run on sonnet
/// (the designated best coding model — opus already did the design in planning).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    Opus,
    Sonnet,
}

impl ModelTier {
    /// The `--model` value the CLI accepts for this tier.
    pub fn as_flag(self) -> &'static str {
        match self {
            ModelTier::Opus => "opus",
            ModelTier::Sonnet => "sonnet",
        }
    }
}

/// How a collab run terminated, beyond the phase it reached. Recognized
/// EXTERNAL account-wide conditions (Claude session/rate limit) are excluded
/// from the corpus row set and re-run rather than corrupting an n>=8 data point
/// as a false FAILED; all other worker/infra aborts remain invalid errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunDisposition {
    /// Reached a terminal phase (`CodingComplete`/`CodingFailed`) normally.
    Terminal,
    /// A worker process died from an EXTERNAL session/rate-limit condition. The
    /// run is EXCLUDED from the corpus row set and must be re-run; it is NOT a
    /// task failure.
    ExcludedRetryable,
}

/// Case-insensitive signatures of an EXTERNAL Claude account-wide session/rate
/// limit condition. These are deliberately Claude/API-shaped phrases, not
/// generic 429 wording, so a genuine task failure that prints "Too Many
/// Requests" or "rate limit exceeded" is NOT silently dropped from the corpus.
/// Matched against the worker-failure error string, which (per Gap 1) includes
/// the worker's stdout tail where these messages appear.
const SESSION_LIMIT_SIGNATURES: &[&str] = &[
    "claude usage limit reached",
    "usage limit reached. resets at",
    "you've hit your session limit",
    "\"type\":\"rate_limit_error\"",
    "\"type\": \"rate_limit_error\"",
];

/// True iff `msg` bears an external session/rate-limit signature. Such a worker
/// failure is EXCLUDED + retryable, never a task FAILED.
pub fn is_session_limit_error(msg: &str) -> bool {
    let lower = msg.to_ascii_lowercase();
    SESSION_LIMIT_SIGNATURES.iter().any(|s| lower.contains(s))
}

/// One dispatch decision for a `(phase, owner)` poll. Frozen mirror of the
/// dispatch matrix in `.claude-plugin/commands/collab.md` (owner-first dispatch).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAction {
    /// A single Claude turn that sends its phase event directly. `mode` is
    /// substituted into `$MODE` for templates that still branch. `model` pins the
    /// turn's `--model` tier.
    ClaudeSend {
        template: &'static str,
        mode: &'static str,
        model: ModelTier,
    },
    /// A Claude compose turn (writes an artifact, returns a `ref:`), then an
    /// auto-approved `collab-turn-submit.md` send of `topic` by that ref. `model`
    /// pins the compose turn's tier; the follow-on submit is always mechanical
    /// (sonnet), pinned at the call site.
    ClaudeCompose {
        template: &'static str,
        topic: &'static str,
        model: ModelTier,
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
                model: ModelTier::Opus,
            },
            "PlanSynthesisPending" => WorkerAction::ClaudeSend {
                template: "collab-turn-plan-synthesis.md",
                mode: "send",
                model: ModelTier::Opus,
            },
            "PlanClaudeFinalizePending" => WorkerAction::ClaudeCompose {
                template: "collab-turn-plan-finalize.md",
                topic: "final",
                model: ModelTier::Opus,
            },
            "PlanLocked" => WorkerAction::TaskListBridge,
            "CodeImplementPending" => WorkerAction::ClaudeSend {
                template: "collab-turn-code-implement.md",
                mode: "send",
                model: ModelTier::Sonnet,
            },
            "CodeReviewLocalPending" => WorkerAction::ClaudeSend {
                template: "collab-turn-review-local.md",
                mode: "send",
                model: ModelTier::Opus,
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
/// plan_round, global_round)` key before the run is declared stalled. Most
/// productive turns flip at least one component (owner flip within a phase, phase
/// advance, or review-round bump). The key deliberately omits `fix_commits`, so a
/// commit-only Codex rework turn within `CodeReviewFixGlobalPending` can make
/// progress without moving it — which is why `STUCK_LIMIT > 1`, giving such a turn
/// room to then flip the owner/phase. A key that repeats `STUCK_LIMIT` times means
/// the session is genuinely wedged (hung/looping turn or a submit that never
/// lands), not merely mid-rework. Bailing here bounds wasted work to ~`STUCK_LIMIT`
/// turns instead of grinding the full `MAX_TURNS`.
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
    /// The worker's *printed text* — the `--output-format stream-json --verbose`
    /// transcript's terminal `result` event's `result` field, NOT the raw JSONL
    /// wrappers. Sentinel lines parsed by [`parse_session_id`]/[`parse_ref_line`]
    /// live here. See `collab_live::worker_text_and_usage`.
    pub stdout: String,
    /// True iff this turn's transcript was unparseable and `usage` is a fallback
    /// ZERO (the turn's real Claude tokens are unknown). The driver excludes a
    /// completed run with any such turn — see [`accumulate_claude`] and the
    /// undercount guard in [`run_collab_task`].
    pub usage_unparseable: bool,
    /// True iff the terminal stream-json result envelope carried `is_error:true`.
    /// The worker process may still exit 0 in this case, so the driver must treat
    /// it as a worker abort after preserving this turn's usage.
    pub is_error: bool,
}

/// Result of one spawned Codex turn. `commits_added` is the count of commits the
/// turn added to the worktree (used for `fix_commits` on rework turns).
pub struct CodexResult {
    pub commits_added: u32,
}

/// Spawn workers (claude `-p` / `codex exec`). Injected so the loop is tested
/// with a fake; the prod impl lives in `collab_live.rs`.
pub trait WorkerSpawner {
    fn spawn_claude(&self, prompt: &str, worktree: &Path, model: ModelTier)
        -> Result<WorkerResult>;
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
    /// How the run ended — distinguishes a normal terminal phase from an
    /// external retryable worker abort. See [`RunDisposition`].
    pub disposition: RunDisposition,
}

/// Fold one Claude worker turn into the run totals: accumulate its usage AND
/// carry forward whether its usage was unparseable. Centralized so every spawn
/// site keeps the undercount flag in lockstep with the token sum (a site that
/// only `add_assign`ed the usage would silently drop a drifted turn's flag).
fn accumulate_claude(claude_usage: &mut Usage, any_unparseable: &mut bool, r: &WorkerResult) {
    claude_usage.add_assign(&r.usage);
    *any_unparseable |= r.usage_unparseable;
}

fn worker_result_error(site: WorkerFailureSite, r: &WorkerResult) -> Option<DriveError> {
    r.is_error.then(|| {
        DriveError::Worker(WorkerFailure {
            site,
            source: anyhow!(
                "claude worker terminal result had is_error=true — stdout tail: {}",
                r.stdout
            ),
        })
    })
}

/// Run one task through the headless collab dispatcher loop.
pub fn run_collab_task<R: CollabStateReader, S: WorkerSpawner, A: CodexAttributor>(
    ctx: &CollabTaskCtx,
    reader: &R,
    spawner: &S,
    attributor: &A,
) -> Result<CollabRunResult> {
    // Synthetic, un-pushed PR URL. Must be `https://`-schemed: the server's
    // `final_review` event validation (collab_events.rs) rejects any other scheme
    // (a `javascript:`/`file://` guard), so a `local://` URL is refused and the
    // FinalReview event never lands. The reserved `.invalid` TLD (RFC 6761) keeps
    // it unmistakably fake — nothing is created or pushed.
    let synthetic_pr = format!("https://abeval.invalid/{}", ctx.task_id);

    let mut acc = RunAccum {
        claude_usage: Usage::default(),
        any_usage_unparseable: false,
        fix_commits: 0,
        last_state: None,
    };

    // Drive the bootstrap + dispatcher loop. A spawned-worker process failure
    // must NOT abort the whole run via `?` — that loses every token spent so far
    // and reports an EXTERNAL session-limit kill identically to an infra crash.
    // Catch worker failures here, classify recognized external session/rate
    // limits as EXCLUDED + retryable, and surface partial usage. Every other
    // worker/structural/anomaly/stall error stays a hard `Err` (INVALID, not a
    // data point).
    match drive_collab_loop(ctx, reader, spawner, &synthetic_pr, &mut acc) {
        Ok(()) => {}
        Err(DriveError::Invalid(e)) => return Err(e),
        Err(DriveError::Worker(failure)) => {
            let failure_msg = format!("{:#}", failure.source);
            if !matches!(
                failure.site,
                WorkerFailureSite::BootstrapClaude | WorkerFailureSite::ClaudeTurn
            ) || !is_session_limit_error(&failure_msg)
            {
                return Err(anyhow!(
                    "collab worker failure at {:?} for task {} is not a recognized \
                     external session-limit abort — INVALID run (infra/worker failure, \
                     not a measured task outcome): {}",
                    failure.site,
                    ctx.task_id,
                    failure_msg
                ));
            };
            let disposition = RunDisposition::ExcludedRetryable;
            // Best-effort Codex attribution so partial Codex spend is captured
            // too. This must fail loud: writing a measured zero on attribution
            // failure would defeat the audit trail this sidecar exists to preserve.
            let codex_usage = attributor.attribute().map_err(|ae| {
                anyhow!(
                    "collab run for task {} hit an external session-limit abort, but \
                     Codex token attribution failed while preserving partial spend — \
                     INVALID run: {ae:#}",
                    ctx.task_id
                )
            })?;
            let (reached_phase, review_rounds) = match &acc.last_state {
                Some(s) => (s.phase.clone(), s.global_review_round),
                None => ("WorkerAborted".to_string(), 0),
            };
            eprintln!(
                "abeval: collab run for task {} aborted after a worker failure \
                 ({disposition:?}); preserving partial Claude tokens={}: {}",
                ctx.task_id,
                acc.claude_usage.total(),
                failure_msg
            );
            return Ok(CollabRunResult {
                claude_usage: acc.claude_usage,
                codex_usage,
                reached_phase,
                review_rounds,
                fix_commits: acc.fix_commits,
                pr_url_synthetic: synthetic_pr,
                disposition,
            });
        }
    }

    let RunAccum {
        claude_usage,
        any_usage_unparseable,
        fix_commits,
        last_state,
    } = acc;
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
    // Partial-undercount guard: even with a non-zero total, a single worker turn
    // whose transcript was unparseable means `claude_usage` is missing that turn's
    // real tokens. A completed run is headline-eligible, so a known undercount must
    // exclude it — the all-zero guard above cannot see a partial loss.
    if reached_phase == PHASE_CODING_COMPLETE && any_usage_unparseable {
        return Err(anyhow!(
            "collab run for task {} reached CodingComplete but ≥1 Claude worker turn had \
             an unparseable usage transcript — Claude tokens are undercounted; INVALID \
             run (excluded)",
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
        disposition: RunDisposition::Terminal,
    })
}

/// Mutable run state threaded through [`drive_collab_loop`] so that, when a
/// worker turn aborts the loop, [`run_collab_task`] still holds the partial
/// totals (rather than losing them to `?` propagation).
struct RunAccum {
    claude_usage: Usage,
    any_usage_unparseable: bool,
    fix_commits: u32,
    last_state: Option<SessionState>,
}

/// Where a spawned worker failed. Non-limit failures stay INVALID so bootstrap,
/// Codex CLI, timeout, and config problems do not poison corpus metrics as task
/// outcomes.
#[derive(Debug, Clone, Copy)]
enum WorkerFailureSite {
    BootstrapClaude,
    ClaudeTurn,
    CodexTurn,
}

/// A spawned worker process failure plus enough context to decide whether it is
/// an external session-limit abort or an invalid infra/worker failure.
struct WorkerFailure {
    site: WorkerFailureSite,
    source: anyhow::Error,
}

/// Error class from the dispatcher loop. A [`DriveError::Worker`] is a spawned
/// worker process failure; only recognized session-limit signatures become
/// EXCLUDED. A [`DriveError::Invalid`] is a structural/anomaly/stall/infra
/// condition and propagates as a hard `Err`.
enum DriveError {
    Worker(WorkerFailure),
    Invalid(anyhow::Error),
}

/// Bootstrap the session and run the dispatcher loop, folding each turn's usage
/// into `acc`. Returns `Ok(())` when the loop ends (terminal phase reached, or
/// `MAX_TURNS` exhausted — the caller's terminal-phase guard decides validity).
/// Worker spawn failures map to [`DriveError::Worker`]; every other failure
/// (DB read, render, anomaly, stall, blocker) maps to [`DriveError::Invalid`].
fn drive_collab_loop<R: CollabStateReader, S: WorkerSpawner>(
    ctx: &CollabTaskCtx,
    reader: &R,
    spawner: &S,
    synthetic_pr: &str,
    acc: &mut RunAccum,
) -> std::result::Result<(), DriveError> {
    let wt = ctx.worktree.as_path();

    // (1) Bootstrap: collab_start + print ABEVAL_SESSION_ID=<id>. Mechanical
    // (one tool call + one sentinel line) → sonnet.
    let boot = spawner
        .spawn_claude(&ctx.bootstrap_prompt, wt, ModelTier::Sonnet)
        .map_err(|e| {
            DriveError::Worker(WorkerFailure {
                site: WorkerFailureSite::BootstrapClaude,
                source: e,
            })
        })?;
    accumulate_claude(&mut acc.claude_usage, &mut acc.any_usage_unparseable, &boot);
    if let Some(e) = worker_result_error(WorkerFailureSite::BootstrapClaude, &boot) {
        return Err(e);
    }
    let session_id = parse_session_id(&boot.stdout).map_err(DriveError::Invalid)?;

    // (2) Dispatcher loop.
    // Stall guard: the `(phase, owner, plan_round, global_round)` key last
    // dispatched, and how many consecutive turns have left it unchanged.
    let mut stall_key: Option<(String, String, u32, u32)> = None;
    let mut stall_count: usize = 0;
    for _ in 0..MAX_TURNS {
        let state = reader.read(&session_id).map_err(DriveError::Invalid)?;
        let key = (
            state.phase.clone(),
            state.current_owner.clone(),
            state.review_round,
            state.global_review_round,
        );
        if stall_key.as_ref() == Some(&key) {
            stall_count += 1;
            if stall_count >= STUCK_LIMIT {
                return Err(DriveError::Invalid(anyhow!(
                    "collab run for task {} stalled: phase {} (owner {}, plan round {}, global round {}) did not \
                     advance after {} consecutive worker turns — INVALID run (hung or \
                     looping turn / submit never landed)",
                    ctx.task_id,
                    state.phase,
                    state.current_owner,
                    state.review_round,
                    state.global_review_round,
                    STUCK_LIMIT
                )));
            }
        } else {
            stall_count = 0;
            stall_key = Some(key);
        }
        let action = worker_action(&state.phase, &state.current_owner, state.review_round);
        acc.last_state = Some(state.clone());
        match action {
            WorkerAction::Terminal => break,
            WorkerAction::Anomaly => {
                return Err(DriveError::Invalid(anyhow!(
                    "collab anomaly: phase {} owned by {} (unexpected)",
                    state.phase,
                    state.current_owner
                )));
            }
            WorkerAction::ClaudeSend {
                template,
                mode,
                model,
            } => {
                let prompt = render_worker_prompt(
                    &ctx.prompts_dir,
                    template,
                    &[
                        ("$SESSION_ID", &session_id),
                        ("$BRANCH", &ctx.branch),
                        ("$MODE", mode),
                    ],
                )
                .map_err(DriveError::Invalid)?;
                let r = spawner.spawn_claude(&prompt, wt, model).map_err(|e| {
                    DriveError::Worker(WorkerFailure {
                        site: WorkerFailureSite::ClaudeTurn,
                        source: e,
                    })
                })?;
                accumulate_claude(&mut acc.claude_usage, &mut acc.any_usage_unparseable, &r);
                if let Some(e) = worker_result_error(WorkerFailureSite::ClaudeTurn, &r) {
                    return Err(e);
                }
            }
            WorkerAction::ClaudeCompose {
                template,
                topic,
                model,
            } => {
                let compose = render_worker_prompt(
                    &ctx.prompts_dir,
                    template,
                    &[
                        ("$SESSION_ID", &session_id),
                        ("$BRANCH", &ctx.branch),
                        ("$MODE", "compose"),
                        ("$TOPIC", topic),
                    ],
                )
                .map_err(DriveError::Invalid)?;
                // Snapshot the newest staged drawer before the turn so we can
                // identify the one this compose persists (rowid advances).
                let before_rowid = reader
                    .newest_draft_drawer(i64::MIN)
                    .map_err(DriveError::Invalid)?
                    .map(|(_, rowid)| rowid)
                    .unwrap_or(i64::MIN);
                let cr = spawner.spawn_claude(&compose, wt, model).map_err(|e| {
                    DriveError::Worker(WorkerFailure {
                        site: WorkerFailureSite::ClaudeTurn,
                        source: e,
                    })
                })?;
                accumulate_claude(&mut acc.claude_usage, &mut acc.any_usage_unparseable, &cr);
                if let Some(e) = worker_result_error(WorkerFailureSite::ClaudeTurn, &cr) {
                    return Err(e);
                }
                let artifact_ref = resolve_compose_ref(reader, &cr.stdout, before_rowid, topic)
                    .map_err(DriveError::Invalid)?;
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
                )
                .map_err(DriveError::Invalid)?;
                // The submit is a mechanical event send → sonnet, regardless of the
                // compose turn's tier.
                let sr = spawner
                    .spawn_claude(&submit, wt, ModelTier::Sonnet)
                    .map_err(|e| {
                        DriveError::Worker(WorkerFailure {
                            site: WorkerFailureSite::ClaudeTurn,
                            source: e,
                        })
                    })?;
                accumulate_claude(&mut acc.claude_usage, &mut acc.any_usage_unparseable, &sr);
                if let Some(e) = worker_result_error(WorkerFailureSite::ClaudeTurn, &sr) {
                    return Err(e);
                }
            }
            WorkerAction::TaskListBridge => {
                let prompt = render_worker_prompt(
                    &ctx.prompts_dir,
                    "collab-turn-task-list.md",
                    &[("$SESSION_ID", &session_id), ("$BRANCH", &ctx.branch)],
                )
                .map_err(DriveError::Invalid)?;
                // Mechanical bridge (parse approved plan markdown → send task_list).
                let r = spawner
                    .spawn_claude(&prompt, wt, ModelTier::Sonnet)
                    .map_err(|e| {
                        DriveError::Worker(WorkerFailure {
                            site: WorkerFailureSite::ClaudeTurn,
                            source: e,
                        })
                    })?;
                accumulate_claude(&mut acc.claude_usage, &mut acc.any_usage_unparseable, &r);
                if let Some(e) = worker_result_error(WorkerFailureSite::ClaudeTurn, &r) {
                    return Err(e);
                }
                if let Some(blocker) = parse_blocker_line(&r.stdout) {
                    return Err(DriveError::Invalid(anyhow!(
                        "task_list bridge for task {} returned blocker: {} — INVALID run",
                        ctx.task_id,
                        blocker
                    )));
                }
            }
            WorkerAction::Codex => {
                let cr = spawner.spawn_codex(&session_id, wt).map_err(|e| {
                    DriveError::Worker(WorkerFailure {
                        site: WorkerFailureSite::CodexTurn,
                        source: e,
                    })
                })?;
                if state.phase == "CodeReviewFixGlobalPending" {
                    acc.fix_commits = acc.fix_commits.saturating_add(cr.commits_added);
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
                )
                .map_err(DriveError::Invalid)?;
                // Final-review compose is a review turn → opus; the synthetic submit
                // is a mechanical event send → sonnet.
                let cr = spawner
                    .spawn_claude(&compose, wt, ModelTier::Opus)
                    .map_err(|e| {
                        DriveError::Worker(WorkerFailure {
                            site: WorkerFailureSite::ClaudeTurn,
                            source: e,
                        })
                    })?;
                accumulate_claude(&mut acc.claude_usage, &mut acc.any_usage_unparseable, &cr);
                if let Some(e) = worker_result_error(WorkerFailureSite::ClaudeTurn, &cr) {
                    return Err(e);
                }
                let submit = SYNTHETIC_FINAL_SUBMIT
                    .replace("$SESSION_ID", &session_id)
                    .replace("$PR_URL", synthetic_pr);
                let sr = spawner
                    .spawn_claude(&submit, wt, ModelTier::Sonnet)
                    .map_err(|e| {
                        DriveError::Worker(WorkerFailure {
                            site: WorkerFailureSite::ClaudeTurn,
                            source: e,
                        })
                    })?;
                accumulate_claude(&mut acc.claude_usage, &mut acc.any_usage_unparseable, &sr);
                if let Some(e) = worker_result_error(WorkerFailureSite::ClaudeTurn, &sr) {
                    return Err(e);
                }
            }
        }
    }
    Ok(())
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
        fn spawn_claude(
            &self,
            _prompt: &str,
            _worktree: &Path,
            _model: ModelTier,
        ) -> Result<WorkerResult> {
            // Only the bootstrap turn hits this path here; emit the session sentinel.
            Ok(WorkerResult {
                usage: Usage::default(),
                stdout: "ABEVAL_SESSION_ID=s-stuck\n".to_string(),
                usage_unparseable: false,
                is_error: false,
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

    /// Reader that returns a fixed sequence of states (one per `read` call),
    /// repeating the last once exhausted. Lets a test drive the dispatcher through
    /// an exact phase trajectory to pin the stall-count reset boundary.
    struct SequenceReader {
        states: Vec<SessionState>,
        idx: std::cell::Cell<usize>,
    }
    impl CollabStateReader for SequenceReader {
        fn read(&self, _session_id: &str) -> Result<SessionState> {
            let i = self.idx.get();
            self.idx.set(i + 1);
            let last = self.states.len() - 1;
            Ok(self.states[i.min(last)].clone())
        }
        fn newest_draft_drawer(&self, _after_rowid: i64) -> Result<Option<(String, i64)>> {
            Ok(None)
        }
    }

    /// Spawner whose bootstrap emits the session sentinel AND non-zero usage (so a
    /// run that reaches `CodingComplete` isn't rejected as zero-Claude). Codex turns
    /// add no commits.
    struct UsageSpawner;
    impl WorkerSpawner for UsageSpawner {
        fn spawn_claude(
            &self,
            _prompt: &str,
            _worktree: &Path,
            _model: ModelTier,
        ) -> Result<WorkerResult> {
            Ok(WorkerResult {
                usage: Usage {
                    output_tokens: 10,
                    ..Usage::default()
                },
                stdout: "ABEVAL_SESSION_ID=s-seq\n".to_string(),
                usage_unparseable: false,
                is_error: false,
            })
        }
        fn spawn_codex(&self, _session_id: &str, _worktree: &Path) -> Result<CodexResult> {
            Ok(CodexResult { commits_added: 0 })
        }
    }

    /// Spawner that reaches `CodingComplete` with NON-ZERO usage but flags every
    /// turn's usage as unparseable — the partial-undercount case (`total > 0`, yet
    /// the turn's real Claude tokens are unknown). Exercises the undercount guard
    /// past the all-zero guard.
    struct UnparseableUsageSpawner;
    impl WorkerSpawner for UnparseableUsageSpawner {
        fn spawn_claude(
            &self,
            _prompt: &str,
            _worktree: &Path,
            _model: ModelTier,
        ) -> Result<WorkerResult> {
            Ok(WorkerResult {
                usage: Usage {
                    output_tokens: 10,
                    ..Usage::default()
                },
                stdout: "ABEVAL_SESSION_ID=s-bad\n".to_string(),
                usage_unparseable: true,
                is_error: false,
            })
        }
        fn spawn_codex(&self, _session_id: &str, _worktree: &Path) -> Result<CodexResult> {
            Ok(CodexResult { commits_added: 0 })
        }
    }

    /// Attributor that yields non-zero Codex usage so a completed run passes the
    /// zero-Codex INVALID guard.
    struct NonZeroAttributor;
    impl CodexAttributor for NonZeroAttributor {
        fn attribute(&self) -> Result<Usage> {
            Ok(Usage {
                output_tokens: 5,
                ..Usage::default()
            })
        }
    }

    #[test]
    fn stall_count_resets_when_key_advances_then_repeats() {
        // Trajectory [A, A, B, terminal]: the key A repeats exactly once (one short
        // of STUCK_LIMIT), then B advances the key — which must RESET the counter,
        // not accumulate toward a false stall bail. The run then reaches a terminal
        // phase and succeeds. A naive non-resetting counter would have bailed at B.
        let a = pinned_state("CodeReviewFixGlobalPending", "codex");
        let mut b = pinned_state("CodeReviewFixGlobalPending", "codex");
        b.global_review_round = 1; // distinct key component → counter must reset
        let terminal = pinned_state(PHASE_CODING_COMPLETE, "claude");
        let reader = SequenceReader {
            states: vec![a.clone(), a, b, terminal],
            idx: std::cell::Cell::new(0),
        };
        let ctx = CollabTaskCtx {
            task_id: "abeval-seq".into(),
            worktree: PathBuf::from("/tmp/nonexistent-wt"),
            branch: "abeval/seq".into(),
            prompts_dir: PathBuf::from("/tmp/nonexistent-prompts"),
            bootstrap_prompt: "boot".into(),
        };
        let res = run_collab_task(&ctx, &reader, &UsageSpawner, &NonZeroAttributor)
            .expect("key advance must reset the stall counter, not bail");
        assert_eq!(res.reached_phase, PHASE_CODING_COMPLETE);
    }

    #[test]
    fn completed_run_with_unparseable_worker_usage_is_invalid() {
        // Same completing trajectory as above, but the (bootstrap) Claude turn
        // reports usage_unparseable=true with NON-ZERO usage. The run reaches
        // CodingComplete and clears the zero-Claude and zero-Codex guards, yet must
        // still be INVALID because its Claude total is a known undercount — the
        // partial-loss case the all-zero guard cannot see.
        let a = pinned_state("CodeReviewFixGlobalPending", "codex");
        let mut b = pinned_state("CodeReviewFixGlobalPending", "codex");
        b.global_review_round = 1;
        let terminal = pinned_state(PHASE_CODING_COMPLETE, "claude");
        let reader = SequenceReader {
            states: vec![a.clone(), a, b, terminal],
            idx: std::cell::Cell::new(0),
        };
        let ctx = CollabTaskCtx {
            task_id: "abeval-undercount".into(),
            worktree: PathBuf::from("/tmp/nonexistent-wt"),
            branch: "abeval/undercount".into(),
            prompts_dir: PathBuf::from("/tmp/nonexistent-prompts"),
            bootstrap_prompt: "boot".into(),
        };
        let err = run_collab_task(&ctx, &reader, &UnparseableUsageSpawner, &NonZeroAttributor)
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("INVALID"), "must mark INVALID: {msg}");
        assert!(
            msg.contains("undercount"),
            "must name the undercount cause: {msg}"
        );
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
