//! Pure state machine for the bounded Claude↔Codex planning + coding flow.
//!
//! v1 covers planning: `PlanParallelDrafts` → `PlanSynthesisPending`
//! → `PlanCodexReviewPending` → `PlanClaudeFinalizePending` → `PlanLocked`.
//!
//! v2 extends `PlanLocked` with a human-approved coding loop. A single
//! Claude `task_list` send transitions out of `PlanLocked` into the per-task
//! 5-phase debate; after all tasks, the session enters a local review and a
//! 2-pass global Codex review before landing in `PrReadyPending`, then
//! `CodingComplete` (terminal) on success or `CodingFailed` (terminal) on
//! unrecoverable drift / tooling failure.

pub mod queue;

use std::fmt;

/// Maximum number of review cycles Codex may run on the canonical plan.
/// After this many reviews, Claude is forced into finalize regardless of the
/// verdict (she always gets the last word).
pub const MAX_REVIEW_ROUNDS: u8 = 2;

/// Maximum number of Codex-review debate rounds per coding task. At the cap,
/// Claude's `verdict=disagree_with_reasons` skips Debate and lands directly
/// in `CodeFinalPending`, which advances the task instead of looping back.
pub const MAX_TASK_REVIEW_ROUNDS: u8 = 2;

/// Maximum number of Codex disagree rounds during global review. At the cap,
/// `CodeReviewFinalPending` advances straight to `PrReadyPending` instead of
/// looping back for another Codex pass.
pub const MAX_GLOBAL_REVIEW_ROUNDS: u8 = 2;

/// Prefix on `coding_failure` that marks a failure as "branch drift" — a
/// mismatch the non-owner may detect via its own git ops. Drift failures are
/// the only case where an off-turn agent may emit `FailureReport`; ordinary
/// failures must come from `current_owner` so an off-turn agent cannot
/// unilaterally abort the other agent's work.
pub const BRANCH_DRIFT_PREFIX: &str = "branch_drift:";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    // Planning (v1)
    PlanParallelDrafts,
    PlanSynthesisPending,
    PlanCodexReviewPending,
    PlanClaudeFinalizePending,
    PlanLocked,
    // Coding (v2) — per-task 5-phase debate
    CodeImplementPending,
    CodeReviewPending,
    CodeVerdictPending,
    CodeDebatePending,
    CodeFinalPending,
    // Coding (v2) — local + global review
    CodeReviewLocalPending,
    CodeReviewCodexPending,
    CodeReviewVerdictPending,
    CodeReviewDebatePending,
    CodeReviewFinalPending,
    // Coding (v2) — PR handoff + terminal
    PrReadyPending,
    CodingComplete,
    CodingFailed,
}

impl Phase {
    /// True for phases that permanently end the session. `wait_my_turn` uses
    /// a dynamic terminal set: `PlanLocked` is terminal pre-`task_list`, and
    /// `{CodingComplete, CodingFailed}` is the terminal set post-`task_list`.
    /// This helper returns only the permanently-terminal cases; callers
    /// responsible for the dynamic set check `task_list` on the session.
    pub fn is_terminal_v2(&self) -> bool {
        matches!(self, Self::CodingComplete | Self::CodingFailed)
    }

    /// True if the session is currently inside the v2 coding loop. Used by
    /// `collab_end` to reject early-end calls.
    pub fn is_coding_active(&self) -> bool {
        matches!(
            self,
            Self::CodeImplementPending
                | Self::CodeReviewPending
                | Self::CodeVerdictPending
                | Self::CodeDebatePending
                | Self::CodeFinalPending
                | Self::CodeReviewLocalPending
                | Self::CodeReviewCodexPending
                | Self::CodeReviewVerdictPending
                | Self::CodeReviewDebatePending
                | Self::CodeReviewFinalPending
                | Self::PrReadyPending
        )
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::PlanParallelDrafts => "PlanParallelDrafts",
            Self::PlanSynthesisPending => "PlanSynthesisPending",
            Self::PlanCodexReviewPending => "PlanCodexReviewPending",
            Self::PlanClaudeFinalizePending => "PlanClaudeFinalizePending",
            Self::PlanLocked => "PlanLocked",
            Self::CodeImplementPending => "CodeImplementPending",
            Self::CodeReviewPending => "CodeReviewPending",
            Self::CodeVerdictPending => "CodeVerdictPending",
            Self::CodeDebatePending => "CodeDebatePending",
            Self::CodeFinalPending => "CodeFinalPending",
            Self::CodeReviewLocalPending => "CodeReviewLocalPending",
            Self::CodeReviewCodexPending => "CodeReviewCodexPending",
            Self::CodeReviewVerdictPending => "CodeReviewVerdictPending",
            Self::CodeReviewDebatePending => "CodeReviewDebatePending",
            Self::CodeReviewFinalPending => "CodeReviewFinalPending",
            Self::PrReadyPending => "PrReadyPending",
            Self::CodingComplete => "CodingComplete",
            Self::CodingFailed => "CodingFailed",
        };
        f.write_str(value)
    }
}

impl TryFrom<&str> for Phase {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "PlanParallelDrafts" => Ok(Self::PlanParallelDrafts),
            "PlanSynthesisPending" => Ok(Self::PlanSynthesisPending),
            "PlanCodexReviewPending" => Ok(Self::PlanCodexReviewPending),
            "PlanClaudeFinalizePending" => Ok(Self::PlanClaudeFinalizePending),
            "PlanLocked" => Ok(Self::PlanLocked),
            "CodeImplementPending" => Ok(Self::CodeImplementPending),
            "CodeReviewPending" => Ok(Self::CodeReviewPending),
            "CodeVerdictPending" => Ok(Self::CodeVerdictPending),
            "CodeDebatePending" => Ok(Self::CodeDebatePending),
            "CodeFinalPending" => Ok(Self::CodeFinalPending),
            "CodeReviewLocalPending" => Ok(Self::CodeReviewLocalPending),
            "CodeReviewCodexPending" => Ok(Self::CodeReviewCodexPending),
            "CodeReviewVerdictPending" => Ok(Self::CodeReviewVerdictPending),
            "CodeReviewDebatePending" => Ok(Self::CodeReviewDebatePending),
            "CodeReviewFinalPending" => Ok(Self::CodeReviewFinalPending),
            "PrReadyPending" => Ok(Self::PrReadyPending),
            "CodingComplete" => Ok(Self::CodingComplete),
            "CodingFailed" => Ok(Self::CodingFailed),
            other => Err(format!("unknown collab phase: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabSession {
    pub id: String,
    pub phase: Phase,
    pub current_owner: String,
    pub claude_draft_hash: Option<String>,
    pub codex_draft_hash: Option<String>,
    pub canonical_plan_hash: Option<String>,
    pub final_plan_hash: Option<String>,
    pub codex_review_verdict: Option<String>,
    pub review_round: u8,
    // v2 coding fields. `tasks_count` is not stored — it is derived from
    // `task_list` via `tasks_count_from_list` so there is a single source of
    // truth for task cardinality.
    pub task_list: Option<String>,
    pub current_task_index: Option<u32>,
    pub task_review_round: u8,
    pub global_review_round: u8,
    pub base_sha: Option<String>,
    pub last_head_sha: Option<String>,
    pub pr_url: Option<String>,
    pub coding_failure: Option<String>,
}

impl CollabSession {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            phase: Phase::PlanParallelDrafts,
            current_owner: "claude".to_string(),
            claude_draft_hash: None,
            codex_draft_hash: None,
            canonical_plan_hash: None,
            final_plan_hash: None,
            codex_review_verdict: None,
            review_round: 0,
            task_list: None,
            current_task_index: None,
            task_review_round: 0,
            global_review_round: 0,
            base_sha: None,
            last_head_sha: None,
            pr_url: None,
            coding_failure: None,
        }
    }

    /// Task cardinality derived from the stored `task_list` JSON. Canonical
    /// shape is `{"tasks":[…]}`; any other shape yields `None`. Returns `None`
    /// when `task_list` is unset (pre-`SubmitTaskList`).
    pub fn tasks_count(&self) -> Option<u32> {
        tasks_count_from_list(self.task_list.as_deref())
    }
}

/// Count tasks in a stored `task_list` JSON payload. Canonical shape is
/// `{"tasks":[…]}`; anything else is rejected. Kept narrow on purpose so a
/// corrupt payload yields `None` instead of silently advancing the state
/// machine with a wrong count.
pub fn tasks_count_from_list(raw: Option<&str>) -> Option<u32> {
    let raw = raw?;
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    let tasks = value.get("tasks")?.as_array()?;
    u32::try_from(tasks.len()).ok()
}

/// The set of verdicts accepted on v2 coding topics (`verdict`,
/// `verdict_global`, `review_global`). `review_global` uses the same strings
/// even though only Codex sends it — keeping the vocabulary uniform means
/// harness code can share a verdict-parsing helper.
pub const CODING_VERDICTS: [&str; 2] = ["agree", "disagree_with_reasons"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollabEvent {
    // v1 planning
    SubmitDraft {
        content_hash: String,
    },
    PublishCanonical {
        content_hash: String,
    },
    SubmitReview {
        verdict: String,
    },
    PublishFinal {
        content_hash: String,
    },
    // v2 coding
    SubmitTaskList {
        plan_hash: String,
        base_sha: String,
        task_list_json: String,
        tasks_count: u32,
        head_sha: String,
    },
    CodeImplement {
        head_sha: String,
    },
    CodeReview {
        head_sha: String,
    },
    CodeVerdict {
        verdict: String,
        head_sha: String,
    },
    CodeComment {
        head_sha: String,
    },
    CodeFinal {
        head_sha: String,
    },
    ReviewLocal {
        head_sha: String,
    },
    ReviewGlobal {
        verdict: String,
        head_sha: String,
    },
    VerdictGlobal {
        verdict: String,
        head_sha: String,
    },
    CommentGlobal {
        head_sha: String,
    },
    FinalReview {
        head_sha: String,
    },
    PrOpened {
        pr_url: String,
        head_sha: String,
    },
    /// Emitted by either agent when branch drift, gate exhaustion, `gh_auth`,
    /// or any other unrecoverable error occurs during coding. Transitions to
    /// `CodingFailed` from any coding-active phase. Stores `coding_failure`.
    FailureReport {
        coding_failure: String,
    },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CollabError {
    #[error("not your turn: expected {expected}, got {got}")]
    NotYourTurn { expected: String, got: String },

    #[error("draft already submitted by {agent}")]
    AlreadySubmittedDraft { agent: String },

    #[error("invalid verdict value: {0}")]
    InvalidVerdictValue(String),

    #[error("wrong phase: expected {expected}, got {got}")]
    WrongPhase { expected: String, got: String },

    #[error("session is locked")]
    SessionLocked,

    /// `expected` is intentionally elided from the Display string: the
    /// stored `final_plan_hash` must not leak to callers that probe with
    /// arbitrary hashes. The field is retained for structured logging on
    /// the server side.
    #[error("plan_hash mismatch: got {got}")]
    PlanHashMismatch { expected: String, got: String },

    #[error("task_list must contain at least one task")]
    EmptyTaskList,

    #[error("final_plan_hash not set — session has not reached PlanLocked")]
    PlanNotFinalized,

    #[error("base_sha is required")]
    MissingBaseSha,
}

fn event_name(event: &CollabEvent) -> &'static str {
    match event {
        CollabEvent::SubmitDraft { .. } => "SubmitDraft",
        CollabEvent::PublishCanonical { .. } => "PublishCanonical",
        CollabEvent::SubmitReview { .. } => "SubmitReview",
        CollabEvent::PublishFinal { .. } => "PublishFinal",
        CollabEvent::SubmitTaskList { .. } => "SubmitTaskList",
        CollabEvent::CodeImplement { .. } => "CodeImplement",
        CollabEvent::CodeReview { .. } => "CodeReview",
        CollabEvent::CodeVerdict { .. } => "CodeVerdict",
        CollabEvent::CodeComment { .. } => "CodeComment",
        CollabEvent::CodeFinal { .. } => "CodeFinal",
        CollabEvent::ReviewLocal { .. } => "ReviewLocal",
        CollabEvent::ReviewGlobal { .. } => "ReviewGlobal",
        CollabEvent::VerdictGlobal { .. } => "VerdictGlobal",
        CollabEvent::CommentGlobal { .. } => "CommentGlobal",
        CollabEvent::FinalReview { .. } => "FinalReview",
        CollabEvent::PrOpened { .. } => "PrOpened",
        CollabEvent::FailureReport { .. } => "FailureReport",
    }
}

/// The single `CollabEvent` variant each active phase expects. Used by the
/// catch-all `WrongPhase` arm to build a uniform error message. Terminal
/// phases return a placeholder that the catch-all never reaches because
/// `CodingComplete`/`CodingFailed` short-circuit to `SessionLocked` first.
fn expected_event_for_phase(phase: &Phase) -> &'static str {
    match phase {
        Phase::PlanParallelDrafts => "SubmitDraft",
        Phase::PlanSynthesisPending => "PublishCanonical",
        Phase::PlanCodexReviewPending => "SubmitReview",
        Phase::PlanClaudeFinalizePending => "PublishFinal",
        Phase::PlanLocked => "SubmitTaskList",
        Phase::CodeImplementPending => "CodeImplement",
        Phase::CodeReviewPending => "CodeReview",
        Phase::CodeVerdictPending => "CodeVerdict",
        Phase::CodeDebatePending => "CodeComment",
        Phase::CodeFinalPending => "CodeFinal",
        Phase::CodeReviewLocalPending => "ReviewLocal",
        Phase::CodeReviewCodexPending => "ReviewGlobal",
        Phase::CodeReviewVerdictPending => "VerdictGlobal",
        Phase::CodeReviewDebatePending => "CommentGlobal",
        Phase::CodeReviewFinalPending => "FinalReview",
        Phase::PrReadyPending => "PrOpened",
        Phase::CodingComplete | Phase::CodingFailed => "SessionLocked",
    }
}

/// Require an actor to match the expected value, else return `NotYourTurn`.
fn require_actor(actor: &str, expected: &str) -> Result<(), CollabError> {
    if actor == expected {
        Ok(())
    } else {
        Err(CollabError::NotYourTurn {
            expected: expected.to_string(),
            got: actor.to_string(),
        })
    }
}

/// Validate one of the coding-loop verdict strings.
fn validate_coding_verdict(verdict: &str) -> Result<(), CollabError> {
    if CODING_VERDICTS.contains(&verdict) {
        Ok(())
    } else {
        Err(CollabError::InvalidVerdictValue(verdict.to_string()))
    }
}

/// Apply the per-task advance rule. Resets `task_review_round` and either
/// increments `current_task_index` or transitions into local review.
///
/// `task_list` and `current_task_index` are invariants of every coding-active
/// phase; if either is missing the state machine has already drifted and we
/// panic rather than silently treat it as zero tasks.
fn advance_task(session: &mut CollabSession) {
    session.task_review_round = 0;
    let total = session
        .tasks_count()
        .expect("task_list must be set and well-formed in coding-active phase");
    let current = session
        .current_task_index
        .expect("current_task_index must be set in coding-active phase");
    let next = current.saturating_add(1);
    if next >= total {
        session.phase = Phase::CodeReviewLocalPending;
        session.current_owner = "claude".to_string();
    } else {
        session.current_task_index = Some(next);
        session.phase = Phase::CodeImplementPending;
        session.current_owner = "claude".to_string();
    }
}

pub fn apply_event(
    session: &CollabSession,
    actor: &str,
    event: &CollabEvent,
) -> Result<CollabSession, CollabError> {
    // v2: PlanLocked is transient pre-`task_list`. The ONLY transition out of
    // it is a `SubmitTaskList` from Claude — anything else is rejected as
    // SessionLocked. The terminal coding phases reject all further events.
    if matches!(session.phase, Phase::CodingComplete | Phase::CodingFailed) {
        return Err(CollabError::SessionLocked);
    }

    let mut next = session.clone();

    match (&session.phase, event) {
        (Phase::PlanParallelDrafts, CollabEvent::SubmitDraft { content_hash }) => match actor {
            "claude" => {
                if session.claude_draft_hash.is_some() {
                    return Err(CollabError::AlreadySubmittedDraft {
                        agent: actor.to_string(),
                    });
                }
                next.claude_draft_hash = Some(content_hash.clone());
                if session.codex_draft_hash.is_some() {
                    next.phase = Phase::PlanSynthesisPending;
                    next.current_owner = "claude".to_string();
                } else {
                    next.current_owner = "codex".to_string();
                }
            }
            "codex" => {
                if session.codex_draft_hash.is_some() {
                    return Err(CollabError::AlreadySubmittedDraft {
                        agent: actor.to_string(),
                    });
                }
                next.codex_draft_hash = Some(content_hash.clone());
                if session.claude_draft_hash.is_some() {
                    next.phase = Phase::PlanSynthesisPending;
                    next.current_owner = "claude".to_string();
                } else {
                    next.current_owner = "claude".to_string();
                }
            }
            _ => {
                return Err(CollabError::NotYourTurn {
                    expected: "claude|codex".to_string(),
                    got: actor.to_string(),
                });
            }
        },
        (Phase::PlanSynthesisPending, CollabEvent::PublishCanonical { content_hash }) => {
            require_actor(actor, "claude")?;
            next.canonical_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanCodexReviewPending;
            next.current_owner = "codex".to_string();
        }
        (Phase::PlanCodexReviewPending, CollabEvent::SubmitReview { verdict }) => {
            require_actor(actor, "codex")?;
            if !matches!(
                verdict.as_str(),
                "approve" | "approve_with_minor_edits" | "request_changes"
            ) {
                return Err(CollabError::InvalidVerdictValue(verdict.clone()));
            }
            next.codex_review_verdict = Some(verdict.clone());
            next.review_round = session.review_round.saturating_add(1);

            // request_changes returns to synthesis (Claude revises) unless we've
            // hit the cap — then Claude is forced into finalize with the last word.
            let force_finalize = next.review_round >= MAX_REVIEW_ROUNDS;
            if verdict == "request_changes" && !force_finalize {
                next.phase = Phase::PlanSynthesisPending;
                next.current_owner = "claude".to_string();
            } else {
                next.phase = Phase::PlanClaudeFinalizePending;
                next.current_owner = "claude".to_string();
            }
        }
        (Phase::PlanClaudeFinalizePending, CollabEvent::PublishFinal { content_hash }) => {
            require_actor(actor, "claude")?;
            next.final_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanLocked;
        }
        // ── v2: the one transition out of PlanLocked ──────────────────────
        (
            Phase::PlanLocked,
            CollabEvent::SubmitTaskList {
                plan_hash,
                base_sha,
                task_list_json,
                tasks_count,
                head_sha,
            },
        ) => {
            require_actor(actor, "claude")?;
            let expected = session
                .final_plan_hash
                .as_deref()
                .ok_or(CollabError::PlanNotFinalized)?;
            if plan_hash != expected {
                return Err(CollabError::PlanHashMismatch {
                    expected: expected.to_string(),
                    got: plan_hash.clone(),
                });
            }
            if *tasks_count == 0 {
                return Err(CollabError::EmptyTaskList);
            }
            if base_sha.is_empty() {
                return Err(CollabError::MissingBaseSha);
            }
            next.task_list = Some(task_list_json.clone());
            next.current_task_index = Some(0);
            next.task_review_round = 0;
            next.global_review_round = 0;
            next.base_sha = Some(base_sha.clone());
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeImplementPending;
            next.current_owner = "claude".to_string();
        }
        // ── v2: per-task 5-phase debate ───────────────────────────────────
        (Phase::CodeImplementPending, CollabEvent::CodeImplement { head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewPending;
            next.current_owner = "codex".to_string();
        }
        (Phase::CodeReviewPending, CollabEvent::CodeReview { head_sha }) => {
            require_actor(actor, "codex")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeVerdictPending;
            next.current_owner = "claude".to_string();
        }
        (Phase::CodeVerdictPending, CollabEvent::CodeVerdict { verdict, head_sha }) => {
            require_actor(actor, "claude")?;
            validate_coding_verdict(verdict)?;
            next.last_head_sha = Some(head_sha.clone());
            if verdict == "agree" {
                advance_task(&mut next);
            } else {
                // disagree_with_reasons: bump the debate counter. At cap, skip
                // the Debate phase and go straight to Final — Claude still has
                // the last word but Codex gets no further rebuttal.
                next.task_review_round = session.task_review_round.saturating_add(1);
                if next.task_review_round >= MAX_TASK_REVIEW_ROUNDS {
                    next.phase = Phase::CodeFinalPending;
                    next.current_owner = "claude".to_string();
                } else {
                    next.phase = Phase::CodeDebatePending;
                    next.current_owner = "codex".to_string();
                }
            }
        }
        (Phase::CodeDebatePending, CollabEvent::CodeComment { head_sha }) => {
            require_actor(actor, "codex")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeFinalPending;
            next.current_owner = "claude".to_string();
        }
        (Phase::CodeFinalPending, CollabEvent::CodeFinal { head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            // Read from `next` so the check is robust if a future refactor
            // mutates `next.task_review_round` in this arm. `next` is a fresh
            // clone of `session`, so values are equal today.
            if next.task_review_round >= MAX_TASK_REVIEW_ROUNDS {
                // Round cap reached — force advance instead of looping back.
                advance_task(&mut next);
            } else {
                // Under the cap: loop back so Codex re-reviews Claude's fixes.
                // `task_review_round` is preserved across the loopback so the
                // next CodeVerdictPending→CodeFinalPending cycle sees the
                // incremented counter.
                next.phase = Phase::CodeReviewPending;
                next.current_owner = "codex".to_string();
            }
        }
        // ── v2: local review (Claude solo) ────────────────────────────────
        (Phase::CodeReviewLocalPending, CollabEvent::ReviewLocal { head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewCodexPending;
            next.current_owner = "codex".to_string();
        }
        // ── v2: global Codex review (4-phase, 2-pass) ─────────────────────
        (Phase::CodeReviewCodexPending, CollabEvent::ReviewGlobal { verdict, head_sha }) => {
            require_actor(actor, "codex")?;
            validate_coding_verdict(verdict)?;
            next.last_head_sha = Some(head_sha.clone());
            if verdict == "agree" {
                next.phase = Phase::PrReadyPending;
                next.current_owner = "claude".to_string();
            } else {
                next.global_review_round = session.global_review_round.saturating_add(1);
                next.phase = Phase::CodeReviewVerdictPending;
                next.current_owner = "claude".to_string();
            }
        }
        (Phase::CodeReviewVerdictPending, CollabEvent::VerdictGlobal { verdict, head_sha }) => {
            require_actor(actor, "claude")?;
            validate_coding_verdict(verdict)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewDebatePending;
            next.current_owner = "codex".to_string();
        }
        (Phase::CodeReviewDebatePending, CollabEvent::CommentGlobal { head_sha }) => {
            require_actor(actor, "codex")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewFinalPending;
            next.current_owner = "claude".to_string();
        }
        (Phase::CodeReviewFinalPending, CollabEvent::FinalReview { head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            if session.global_review_round >= MAX_GLOBAL_REVIEW_ROUNDS {
                next.phase = Phase::PrReadyPending;
                next.current_owner = "claude".to_string();
            } else {
                next.phase = Phase::CodeReviewCodexPending;
                next.current_owner = "codex".to_string();
            }
        }
        // ── v2: PR handoff ────────────────────────────────────────────────
        (Phase::PrReadyPending, CollabEvent::PrOpened { pr_url, head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            next.pr_url = Some(pr_url.clone());
            next.phase = Phase::CodingComplete;
            next.current_owner = "claude".to_string();
        }
        // ── v2: failure is valid from any coding-active phase ─────────────
        (phase, CollabEvent::FailureReport { coding_failure }) if phase.is_coding_active() => {
            // Drift failures (prefix `branch_drift:`) may be emitted by either
            // agent because the non-owner often detects drift via its own git
            // ops. Any other failure must come from `current_owner` so an
            // off-turn agent cannot unilaterally abort the other's work.
            let is_drift = coding_failure.starts_with(BRANCH_DRIFT_PREFIX);
            if !is_drift && actor != session.current_owner {
                return Err(CollabError::NotYourTurn {
                    expected: session.current_owner.clone(),
                    got: actor.to_string(),
                });
            }
            next.coding_failure = Some(coding_failure.clone());
            next.phase = Phase::CodingFailed;
            next.current_owner = actor.to_string();
        }
        (phase, _) => {
            // Terminal phases are short-circuited by the guard at the top of
            // this function, so they never reach here. The debug_assert
            // catches any future refactor that reorders or removes the guard.
            debug_assert!(
                !matches!(phase, Phase::CodingComplete | Phase::CodingFailed),
                "terminal phase {phase:?} reached WrongPhase catch-all",
            );
            return Err(CollabError::WrongPhase {
                expected: expected_event_for_phase(phase).to_string(),
                got: event_name(event).to_string(),
            });
        }
    }

    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> CollabSession {
        CollabSession::new("test-session")
    }

    fn draft(actor: &str, hash: &str, s: &CollabSession) -> CollabSession {
        apply_event(
            s,
            actor,
            &CollabEvent::SubmitDraft {
                content_hash: hash.to_string(),
            },
        )
        .unwrap()
    }

    fn canonical(hash: &str, s: &CollabSession) -> CollabSession {
        apply_event(
            s,
            "claude",
            &CollabEvent::PublishCanonical {
                content_hash: hash.to_string(),
            },
        )
        .unwrap()
    }

    fn review(verdict: &str, s: &CollabSession) -> CollabSession {
        apply_event(
            s,
            "codex",
            &CollabEvent::SubmitReview {
                verdict: verdict.to_string(),
            },
        )
        .unwrap()
    }

    /// Run the v1 flow to the point where `final_plan_hash` is set and the
    /// session is `PlanLocked`, ready for `SubmitTaskList`.
    fn locked_session(final_hash: &str) -> CollabSession {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("canonical", &s);
        let s = review("approve", &s);
        apply_event(
            &s,
            "claude",
            &CollabEvent::PublishFinal {
                content_hash: final_hash.to_string(),
            },
        )
        .unwrap()
    }

    /// Build a canonical `{"tasks":[…]}` JSON of `count` placeholder tasks so
    /// the derived `tasks_count_from_list` matches what we pass in the event.
    fn canonical_task_list(count: u32) -> String {
        let tasks: Vec<serde_json::Value> = (0..count)
            .map(|i| {
                serde_json::json!({
                    "id": i as i64 + 1,
                    "title": format!("task-{}", i + 1),
                    "acceptance": ["ok"],
                })
            })
            .collect();
        serde_json::json!({ "tasks": tasks }).to_string()
    }

    fn submit_task_list(s: &CollabSession, plan_hash: &str, tasks_count: u32) -> CollabSession {
        apply_event(
            s,
            "claude",
            &CollabEvent::SubmitTaskList {
                plan_hash: plan_hash.to_string(),
                base_sha: "base0".to_string(),
                task_list_json: canonical_task_list(tasks_count),
                tasks_count,
                head_sha: "head0".to_string(),
            },
        )
        .unwrap()
    }

    // ── v1 regression ────────────────────────────────────────────────────

    #[test]
    fn test_parallel_drafts_both_submit_advances_phase() {
        let s = session();
        let s = draft("claude", "c1", &s);
        assert_eq!(s.phase, Phase::PlanParallelDrafts);
        let s = draft("codex", "c2", &s);
        assert_eq!(s.phase, Phase::PlanSynthesisPending);
        assert_eq!(s.current_owner, "claude");
    }

    #[test]
    fn test_duplicate_draft_rejected() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::SubmitDraft {
                content_hash: "c2".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            CollabError::AlreadySubmittedDraft {
                agent: "claude".to_string()
            }
        );
    }

    #[test]
    fn test_codex_review_approve_advances_to_finalize() {
        for verdict in ["approve", "approve_with_minor_edits"] {
            let s = session();
            let s = draft("claude", "c1", &s);
            let s = draft("codex", "c2", &s);
            let s = canonical("canonical", &s);
            let s = review(verdict, &s);
            assert_eq!(s.phase, Phase::PlanClaudeFinalizePending);
            assert_eq!(s.codex_review_verdict.as_deref(), Some(verdict));
            assert_eq!(s.review_round, 1);
        }
    }

    #[test]
    fn test_request_changes_at_cap_forces_finalize() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("v1", &s);
        let s = review("request_changes", &s);
        let s = canonical("v2", &s);
        let s = review("request_changes", &s);

        assert_eq!(s.review_round, MAX_REVIEW_ROUNDS);
        assert_eq!(s.phase, Phase::PlanClaudeFinalizePending);
    }

    #[test]
    fn test_invalid_verdict_rejected() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("canonical", &s);
        let err = apply_event(
            &s,
            "codex",
            &CollabEvent::SubmitReview {
                verdict: "looks good to me".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            CollabError::InvalidVerdictValue("looks good to me".to_string())
        );
    }

    // ── v2: PlanLocked → task_list transition ────────────────────────────

    #[test]
    fn test_task_list_transitions_to_code_implement() {
        let s = locked_session("hash-final");
        assert_eq!(s.phase, Phase::PlanLocked);
        let s = submit_task_list(&s, "hash-final", 2);
        assert_eq!(s.phase, Phase::CodeImplementPending);
        assert_eq!(s.current_owner, "claude");
        assert_eq!(s.current_task_index, Some(0));
        assert_eq!(s.tasks_count(), Some(2));
        assert_eq!(s.task_review_round, 0);
        assert_eq!(s.global_review_round, 0);
        assert_eq!(s.base_sha.as_deref(), Some("base0"));
        assert_eq!(s.last_head_sha.as_deref(), Some("head0"));
    }

    #[test]
    fn test_task_list_rejects_plan_hash_mismatch() {
        let s = locked_session("hash-final");
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::SubmitTaskList {
                plan_hash: "wrong".to_string(),
                base_sha: "base".to_string(),
                task_list_json: "[]".to_string(),
                tasks_count: 1,
                head_sha: "h".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, CollabError::PlanHashMismatch { .. }));
    }

    #[test]
    fn test_task_list_rejects_empty_tasks() {
        let s = locked_session("hash-final");
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::SubmitTaskList {
                plan_hash: "hash-final".to_string(),
                base_sha: "base".to_string(),
                task_list_json: "[]".to_string(),
                tasks_count: 0,
                head_sha: "h".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(err, CollabError::EmptyTaskList);
    }

    #[test]
    fn test_task_list_rejects_missing_base_sha() {
        let s = locked_session("hash-final");
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::SubmitTaskList {
                plan_hash: "hash-final".to_string(),
                base_sha: "".to_string(),
                task_list_json: "[]".to_string(),
                tasks_count: 1,
                head_sha: "h".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(err, CollabError::MissingBaseSha);
    }

    #[test]
    fn test_task_list_rejected_from_non_claude() {
        let s = locked_session("hash-final");
        let err = apply_event(
            &s,
            "codex",
            &CollabEvent::SubmitTaskList {
                plan_hash: "hash-final".to_string(),
                base_sha: "b".to_string(),
                task_list_json: "[]".to_string(),
                tasks_count: 1,
                head_sha: "h".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, CollabError::NotYourTurn { .. }));
    }

    #[test]
    fn test_task_list_rejected_before_plan_locked() {
        let s = session();
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::SubmitTaskList {
                plan_hash: "x".to_string(),
                base_sha: "b".to_string(),
                task_list_json: "[]".to_string(),
                tasks_count: 1,
                head_sha: "h".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, CollabError::WrongPhase { .. }));
    }

    // ── v2: per-task happy path ──────────────────────────────────────────

    fn happy_task_cycle(s: &CollabSession, head: &str) -> CollabSession {
        let s = apply_event(
            s,
            "claude",
            &CollabEvent::CodeImplement {
                head_sha: head.to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CodeReview {
                head_sha: head.to_string(),
            },
        )
        .unwrap();
        apply_event(
            &s,
            "claude",
            &CollabEvent::CodeVerdict {
                verdict: "agree".to_string(),
                head_sha: head.to_string(),
            },
        )
        .unwrap()
    }

    #[test]
    fn test_two_task_happy_path_reaches_local_review() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 2);
        let s = happy_task_cycle(&s, "h1");
        assert_eq!(s.phase, Phase::CodeImplementPending);
        assert_eq!(s.current_task_index, Some(1));
        assert_eq!(s.task_review_round, 0);

        let s = happy_task_cycle(&s, "h2");
        assert_eq!(s.phase, Phase::CodeReviewLocalPending);
        assert_eq!(s.current_owner, "claude");
        assert_eq!(s.task_review_round, 0);
    }

    #[test]
    fn test_code_implement_wrong_sender_rejected() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        let err = apply_event(
            &s,
            "codex",
            &CollabEvent::CodeImplement {
                head_sha: "h".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, CollabError::NotYourTurn { .. }));
    }

    // ── v2: per-task disagree round + cap ────────────────────────────────

    #[test]
    fn test_task_disagree_round_loops_back_to_review() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        // implement → review → verdict=disagree
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeImplement {
                head_sha: "h1".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CodeReview {
                head_sha: "h1".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeVerdict {
                verdict: "disagree_with_reasons".to_string(),
                head_sha: "h1".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeDebatePending);
        assert_eq!(s.task_review_round, 1);
        assert_eq!(s.current_owner, "codex");

        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CodeComment {
                head_sha: "h2".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeFinalPending);
        assert_eq!(s.current_owner, "claude");

        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeFinal {
                head_sha: "h3".to_string(),
            },
        )
        .unwrap();
        // Under cap: loops back to Review so Codex re-reviews Claude's fixes.
        assert_eq!(s.phase, Phase::CodeReviewPending);
        assert_eq!(s.task_review_round, 1);
    }

    #[test]
    fn test_two_disagrees_force_final_and_advance() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeImplement {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        // Round 1
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CodeReview {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeVerdict {
                verdict: "disagree_with_reasons".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CodeComment {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeFinal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeReviewPending);
        // Round 2: disagree at cap skips Debate and goes straight to Final.
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CodeReview {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeVerdict {
                verdict: "disagree_with_reasons".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeFinalPending);
        assert_eq!(s.task_review_round, MAX_TASK_REVIEW_ROUNDS);

        // Final at cap advances (single-task plan → local review).
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeFinal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeReviewLocalPending);
        assert_eq!(s.task_review_round, 0);
    }

    // ── v2: global review ────────────────────────────────────────────────

    #[test]
    fn test_review_global_agree_goes_directly_to_pr_ready() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        let s = happy_task_cycle(&s, "h");
        assert_eq!(s.phase, Phase::CodeReviewLocalPending);
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::ReviewLocal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeReviewCodexPending);
        assert_eq!(s.current_owner, "codex");

        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::ReviewGlobal {
                verdict: "agree".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::PrReadyPending);
        assert_eq!(s.current_owner, "claude");
        assert_eq!(s.global_review_round, 0);
    }

    #[test]
    fn test_global_review_disagree_round_loops_and_bumps_counter() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        let s = happy_task_cycle(&s, "h");
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::ReviewLocal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::ReviewGlobal {
                verdict: "disagree_with_reasons".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeReviewVerdictPending);
        assert_eq!(s.global_review_round, 1);

        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::VerdictGlobal {
                verdict: "disagree_with_reasons".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CommentGlobal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::FinalReview {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        // Round 1 complete: Final loops back to Codex review.
        assert_eq!(s.phase, Phase::CodeReviewCodexPending);

        // Round 2 disagree → round counter at cap.
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::ReviewGlobal {
                verdict: "disagree_with_reasons".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.global_review_round, MAX_GLOBAL_REVIEW_ROUNDS);

        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::VerdictGlobal {
                verdict: "agree".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CommentGlobal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::FinalReview {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        // Cap reached: Final advances to PR-ready instead of looping.
        assert_eq!(s.phase, Phase::PrReadyPending);
    }

    // ── v2: PR handoff + terminal ────────────────────────────────────────

    #[test]
    fn test_pr_opened_transitions_to_coding_complete() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        let s = happy_task_cycle(&s, "h");
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::ReviewLocal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::ReviewGlobal {
                verdict: "agree".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::PrOpened {
                pr_url: "https://example/pr/1".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodingComplete);
        assert_eq!(s.pr_url.as_deref(), Some("https://example/pr/1"));

        // Terminal: further events rejected.
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeImplement {
                head_sha: "x".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(err, CollabError::SessionLocked);
    }

    #[test]
    fn test_failure_report_from_coding_phase_transitions_to_coding_failed() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        // Drift detected before implement — agent emits FailureReport.
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::FailureReport {
                coding_failure: "branch_drift: expected=abc got=def".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodingFailed);
        assert!(s
            .coding_failure
            .as_deref()
            .unwrap()
            .starts_with("branch_drift:"));

        // Terminal.
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeImplement {
                head_sha: "h".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(err, CollabError::SessionLocked);
    }

    #[test]
    fn test_failure_report_rejected_outside_coding_active_phase() {
        let s = locked_session("hf");
        // PlanLocked is not coding-active → FailureReport falls through to the
        // catch-all WrongPhase arm.
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::FailureReport {
                coding_failure: "nope".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, CollabError::WrongPhase { .. }));
    }

    #[test]
    fn test_failure_report_from_non_owner_requires_branch_drift_prefix() {
        // H2: off-turn FailureReport is only allowed if the reason begins with
        // the branch_drift: prefix. A non-owner submitting an arbitrary
        // coding_failure must be rejected as WrongTurn.
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::CodeImplement {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.current_owner, "codex");

        // Claude (non-owner here) with a non-drift reason → rejected.
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::FailureReport {
                coding_failure: "general failure".to_string(),
            },
        )
        .unwrap_err();
        assert!(matches!(err, CollabError::NotYourTurn { .. }));

        // Same non-owner with a branch_drift: reason → accepted.
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::FailureReport {
                coding_failure: "branch_drift: expected=abc".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodingFailed);
    }

    #[test]
    fn test_failure_report_from_pr_ready_pending_transitions_to_failed() {
        // FailureReport must be accepted in every coding-active phase,
        // including late ones like PrReadyPending and CodeReviewFinalPending.
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        let s = happy_task_cycle(&s, "h");
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::ReviewLocal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::ReviewGlobal {
                verdict: "agree".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::PrReadyPending);

        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::FailureReport {
                coding_failure: "gh pr create failed".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodingFailed);
    }

    #[test]
    fn test_failure_report_from_code_review_final_pending_transitions_to_failed() {
        let s = locked_session("hf");
        let s = submit_task_list(&s, "hf", 1);
        let s = happy_task_cycle(&s, "h");
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::ReviewLocal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::ReviewGlobal {
                verdict: "disagree_with_reasons".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::VerdictGlobal {
                verdict: "agree".to_string(),
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        let s = apply_event(
            &s,
            "codex",
            &CollabEvent::CommentGlobal {
                head_sha: "h".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodeReviewFinalPending);

        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::FailureReport {
                coding_failure: "local gate regressed".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::CodingFailed);
    }

    #[test]
    fn test_tasks_count_from_list_only_accepts_canonical_shape() {
        // Derived tasks_count requires `{"tasks":[...]}`; bare arrays and
        // objects without `tasks` return None.
        let raw = canonical_task_list(3);
        assert_eq!(tasks_count_from_list(Some(&raw)), Some(3));
        assert_eq!(tasks_count_from_list(None), None);
        assert_eq!(tasks_count_from_list(Some("{}")), None);
        // Bare array — rejected by the single derivation path.
        assert_eq!(
            tasks_count_from_list(Some("[{\"id\":1,\"title\":\"t\"}]")),
            None
        );
        // Malformed JSON — swallowed by `ok()` and returns None.
        assert_eq!(tasks_count_from_list(Some("not json")), None);
    }
}
