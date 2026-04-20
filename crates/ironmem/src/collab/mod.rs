//! Pure state machine for the bounded Claude↔Codex planning + coding flow.
//!
//! v1 covers planning: `PlanParallelDrafts` → `PlanSynthesisPending`
//! → `PlanCodexReviewPending` → `PlanClaudeFinalizePending` → `PlanLocked`.
//!
//! v3 extends `PlanLocked` with a human-approved coding loop. A single
//! Claude `task_list` send transitions out of `PlanLocked` into the per-task
//! 3-phase linear flow (`CodeImplementPending` → `CodeReviewFixPending` →
//! `CodeFinalPending`); after all tasks, the session enters the global
//! 3-phase flow (`CodeReviewLocalPending` → `CodeReviewFixGlobalPending` →
//! `CodeReviewFinalPending`) and lands directly in `CodingComplete`
//! (terminal) on success — the final Claude turn opens the PR and carries
//! its URL. `CodingFailed` is the unrecoverable-error terminal.

pub mod queue;

mod error;
mod event;
mod phase;
mod session;
mod state_machine;

pub use error::CollabError;
pub use event::CollabEvent;
pub use phase::Phase;
pub use session::{tasks_count_from_list, CollabSession};
pub use state_machine::apply_event;

/// Prefix on `coding_failure` that marks a failure as "branch drift" — a
/// mismatch the non-owner may detect via its own git ops. Drift failures are
/// the only case where an off-turn agent may emit `FailureReport`; ordinary
/// failures must come from `current_owner` so an off-turn agent cannot
/// unilaterally abort the other agent's work.
pub const BRANCH_DRIFT_PREFIX: &str = "branch_drift:";
