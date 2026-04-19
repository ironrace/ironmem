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
