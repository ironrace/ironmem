//! Pure state machine for the bounded Claude↔Codex planning + coding flow.
//!
//! v1 covers planning: `PlanParallelDrafts` → `PlanSynthesisPending`
//! → `PlanCodexReviewPending` → `PlanClaudeFinalizePending` → `PlanLocked`.
//!
//! v3 extends `PlanLocked` with a human-approved coding loop. A single
//! Claude `task_list` send transitions out of `PlanLocked` into the batch
//! implementation phase (`CodeImplementPending`), where Claude orchestrates
//! per-task subagents (via `superpowers:writing-plans` →
//! `superpowers:subagent-driven-development`) entirely on its side. A
//! single `implementation_done` send jumps to the global 3-phase review
//! flow (`CodeReviewLocalPending` → `CodeReviewFixGlobalPending` →
//! `CodeReviewFinalPending`) and lands directly in `CodingComplete`
//! (terminal) on success — the final Claude turn opens the PR and carries
//! its URL. `CodingFailed` is the unrecoverable-error terminal.

pub mod queue;

mod agent;
mod error;
mod event;
mod phase;
mod session;
mod state_machine;

pub use agent::Agent;
pub use error::CollabError;
pub use event::CollabEvent;
pub use phase::Phase;
pub use session::{tasks_count_from_list, CollabSession};
pub use state_machine::{apply_event, start_global_review_session};

/// Prefix on `coding_failure` that marks a failure as "branch drift" — a
/// mismatch the non-owner may detect via its own git ops.
pub const BRANCH_DRIFT_PREFIX: &str = "branch_drift:";

/// Prefix on `coding_failure` that marks a failure as a Codex MCP
/// dispatch failure observed by Claude during `--implementer=codex`. It
/// shares the off-turn admit path with `branch_drift:` because the
/// non-owner (Claude in this case) is the only agent able to detect
/// that the owner's MCP session never advanced — Codex itself isn't
/// running to emit a regular failure report.
pub const CODEX_DISPATCH_FAILED_PREFIX: &str = "codex_dispatch_failed:";

/// Prefixes on `coding_failure` that may be emitted by a non-owner
/// agent. Ordinary failures must still come from `current_owner` so an
/// off-turn agent cannot unilaterally abort the other agent's work; the
/// carve-out exists for failure modes that are structurally observable
/// only from outside the owner's process.
pub const OFF_TURN_FAILURE_PREFIXES: &[&str] = &[BRANCH_DRIFT_PREFIX, CODEX_DISPATCH_FAILED_PREFIX];
