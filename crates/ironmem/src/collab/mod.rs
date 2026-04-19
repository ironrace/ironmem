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

/// The set of verdicts accepted on v2 coding topics (`verdict`,
/// `verdict_global`, `review_global`). `review_global` uses the same strings
/// even though only Codex sends it — keeping the vocabulary uniform means
/// harness code can share a verdict-parsing helper.
pub const CODING_VERDICTS: [&str; 2] = ["agree", "disagree_with_reasons"];
