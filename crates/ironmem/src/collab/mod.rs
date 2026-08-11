//! State machine for the bounded two-party Claude↔Codex planning + coding flow.
//!
//! **Design boundary:** This collab protocol version is intentionally
//! Claude↔Codex-specific.  Generic harness identity is extensible via
//! [`crate::harness::HarnessId`] and the open [`crate::harness::REGISTRY`];
//! adding a third collab participant would require a new protocol version, not
//! a registry entry.  The [`Agent`] role enum is the compiler-enforced type
//! for collab protocol roles; harness-generic code uses `HarnessId` instead.
//!
//! v1 covers planning: `PlanParallelDrafts` → `PlanSynthesisPending`
//! → `PlanCopilotReviewPending` → `PlanFinalizePending` → `PlanLocked`.
//!
//! v3 extends `PlanLocked` with a human-approved coding loop. A single
//! pilot `task_list` send transitions out of `PlanLocked` into the batch
//! implementation phase (`CodeImplementPending`), where the selected
//! implementer orchestrates per-task subagents (via `iron-build`, or
//! directly when `execution_mode` says so) entirely on its side. A
//! single `implementation_done` send jumps to the global 3-phase review
//! flow (`CodeReviewFixGlobalPending` → `CodeReviewLocalPending` →
//! `CodeReviewFinalPending`) — the copilot reviews the raw
//! post-implementation diff first, then the pilot audits the copilot's
//! commits via `/ultrareview-local`, then the pilot opens the PR — and lands
//! directly in `CodingComplete` (terminal) on success — the final pilot turn
//! opens the PR and carries its URL. "Pilot" is the session's `pilot` agent
//! and "copilot" is its counterpart; the split is per-session, not a fixed
//! Claude/Codex assignment. `CodingFailed` is terminal for this session generation, but not
//! always permanent: a `Tooling`-classified failure with a recorded
//! `failed_from_phase` can be restored to that phase via `ResumeCoding`
//! (the `collab_resume` MCP tool), while a `Terminal`-classified failure
//! (unrecognized causes, `branch_drift:`, `subagent_failure:`, or a
//! recoverable report that exceeded the retry ceiling) is genuinely
//! unrecoverable. See [`failure_class::classify`] and
//! [`MAX_RECOVERY_ATTEMPTS`] for the exact rule.

pub mod handoff;
pub mod queue;

mod agent;
mod error;
mod event;
mod failure_class;
mod phase;
mod session;
mod state_machine;
mod task_list;

pub use agent::{Agent, CollabRoles};
pub use error::CollabError;
pub use event::CollabEvent;
pub use failure_class::{classify, FailureClass};
#[cfg(test)]
pub use handoff::load_or_init_actor_generation;
pub use handoff::{
    claim_handoff_token, issue_or_reuse_handoff, read_actor_generation, ActorGeneration,
    HandoffIssue, PendingHandoff,
};
pub use phase::Phase;
pub use session::{tasks_count_from_list, CollabSession};
pub(crate) use state_machine::copilot;
pub use state_machine::{apply_event, start_global_review_session, MAX_RECOVERY_ATTEMPTS};
pub(crate) use task_list::{
    task_count_from_payload, validate_task_list_body, TaskListValidationError,
};

/// Maximum implementation tasks accepted by one collab session. Larger work
/// must be split into independently executable child issues before collab
/// planning is approved.
pub const MAX_TASKS_PER_COLLAB_ISSUE: u32 = 15;

/// Prefix on `coding_failure` that marks a failure as "branch drift" — a
/// mismatch the non-owner may detect via its own git ops.
pub const BRANCH_DRIFT_PREFIX: &str = "branch_drift:";

/// Prefix on `coding_failure` that marks a failure as a Codex MCP
/// dispatch failure observed by Claude during `--implementer=codex`. It
/// shares the off-turn admit path with `branch_drift:` because the
/// non-owner (Claude in this case) is the only agent able to detect
/// that the owner's MCP session never advanced — Codex itself isn't
/// running to emit a regular failure report. Unlike branch drift, it is
/// admissible only from the phases whose Codex turn Claude actually
/// dispatches — see [`dispatch_failure_phase_admits`].
pub const CODEX_DISPATCH_FAILED_PREFIX: &str = "codex_dispatch_failed:";

/// Prefixes considered for non-owner `failure_report`s. Branch drift is
/// admissible for either reporter from any coding-active phase; Codex
/// dispatch failure is admissible only from Claude, against a Codex-owned
/// turn, in a phase Claude could have dispatched. Use
/// [`off_turn_failure_is_admissible`] rather than treating membership here as
/// sufficient authorization.
pub const OFF_TURN_FAILURE_PREFIXES: &[&str] = &[BRANCH_DRIFT_PREFIX, CODEX_DISPATCH_FAILED_PREFIX];

/// Phases from which a `codex_dispatch_failed:` report can possibly be
/// legitimate — the ones whose Codex-owned turn Claude spawns as an MCP
/// one-shot and can therefore watch fail to return:
///
/// * `CodeImplementPending`, when `implementer == Agent::Codex` — the batch
///   implementation turn Claude dispatched.
/// * `CodeReviewFixGlobalPending` — the copilot's post-implementation fix
///   turn. Combined with the `current_owner == Agent::Codex` requirement in
///   [`off_turn_failure_is_admissible`], this only ever admits a session
///   whose copilot is in fact Codex.
///
/// `CodeReviewLocalPending` and `CodeReviewFinalPending` are deliberately
/// excluded: those are the *pilot's* own audit and PR turns, which Claude
/// never dispatches, so a dispatch-failure report against them is either
/// meaningless or an attempt to seize the turn. Planning and terminal phases
/// are excluded for the same reason — nothing is dispatched from them.
///
/// The match is exhaustive on purpose: a new `Phase` must make an explicit
/// admit/deny decision here rather than inheriting a wildcard.
fn dispatch_failure_phase_admits(phase: Phase, implementer: Agent) -> bool {
    match phase {
        Phase::CodeImplementPending => implementer == Agent::Codex,
        Phase::CodeReviewFixGlobalPending => true,
        Phase::PlanParallelDrafts
        | Phase::PlanSynthesisPending
        | Phase::PlanCopilotReviewPending
        | Phase::PlanFinalizePending
        | Phase::PlanLocked
        | Phase::CodeReviewLocalPending
        | Phase::CodeReviewFinalPending
        | Phase::CodingComplete
        | Phase::CodingFailed => false,
    }
}

/// Whether an agent may report this failure while it is not the current
/// owner. Branch drift is independently observable by either participant, in
/// any phase, so its clause is unconditional.
///
/// A Codex-dispatch failure is narrower on two independent axes.
///
/// **Who** may report it: only the *dispatcher* can observe that a
/// Codex-owned background dispatch never ran, so accepting it from Codex (or
/// while Claude owns the turn) would let a non-owner seize a live Claude
/// turn. The `reporter == Agent::Claude` term below names the **dispatcher**,
/// not the pilot. Claude is unconditionally the dispatcher in this codebase —
/// it is the side that spawns the Codex MCP one-shot — and that role is
/// orthogonal to the session's `pilot`/`copilot` assignment. Under
/// `pilot=codex` Claude is *still* the dispatcher, so this clause is
/// deliberately unaffected by pilot assignment. This function is handed
/// `phase` and `implementer` but never the session or its `pilot`, which is
/// what keeps that independence structural rather than merely intended. Do
/// not "generalize" this literal to `pilot(session)` or `copilot(session)`:
/// doing so would let Codex fabricate a report about its own process and
/// seize a live Claude turn.
///
/// **Where** it may be reported: only from a phase whose Codex turn Claude
/// could have dispatched, per [`dispatch_failure_phase_admits`]. An earlier
/// version of this function took no phase at all, and that was genuinely safe
/// while `CodeReviewLocalPending`/`CodeReviewFinalPending` hardcoded Claude
/// as the expected actor — the carve-out firing there handed the turn to
/// Claude, who was already the expected actor, so nothing was seized. Once
/// those two phases became pilot-owned they can be *Codex*-owned, and a
/// phase-blind carve-out would let Claude — the copilot under `pilot=codex` —
/// fabricate a dispatch failure, take the pilot's audit turn and then its PR
/// turn, and end up auditing its own commits. Since `pilot` and `implementer`
/// are deliberately uncorrelated, that is exactly the self-review the
/// pilot/copilot split exists to prevent. Phase awareness is what closes it.
///
/// A recognized prefix also needs at least one byte of detail. This keeps the
/// pre-dispatch turn gate aligned with the state-machine enforcement.
pub fn off_turn_failure_is_admissible(
    coding_failure: &str,
    reporter: Agent,
    current_owner: Agent,
    phase: Phase,
    implementer: Agent,
) -> bool {
    let has_detail = |prefix: &str| {
        coding_failure
            .strip_prefix(prefix)
            .is_some_and(|detail| !detail.is_empty())
    };

    has_detail(BRANCH_DRIFT_PREFIX)
        || (reporter == Agent::Claude
            && current_owner == Agent::Codex
            && dispatch_failure_phase_admits(phase, implementer)
            && has_detail(CODEX_DISPATCH_FAILED_PREFIX))
}

/// Prefix on `coding_failure` that marks a failed `git commit` — a
/// recoverable tooling failure (see [`failure_class`]).
pub const GIT_COMMIT_FAILED_PREFIX: &str = "git_commit_failed:";

/// Prefix on `coding_failure` that marks a failed `git push` — a
/// recoverable tooling failure (see [`failure_class`]).
pub const GIT_PUSH_FAILED_PREFIX: &str = "git_push_failed:";

/// Prefix on `coding_failure` that marks a sandbox or permission denial
/// encountered by the implementer — a recoverable tooling failure (see
/// [`failure_class`]).
pub const SANDBOX_DENIED_PREFIX: &str = "sandbox_denied:";

/// Prefix on `coding_failure` that marks the implementer running out of
/// disk space — a recoverable tooling failure (see [`failure_class`]).
pub const DISK_FULL_PREFIX: &str = "disk_full:";

/// Prefix on `coding_failure` that marks a transient network failure
/// encountered by the implementer — a recoverable tooling failure (see
/// [`failure_class`]).
pub const NETWORK_FAILED_PREFIX: &str = "network_failed:";

/// Prefixes on `coding_failure` that, when followed by a non-empty detail
/// suffix, classify as [`failure_class::FailureClass::Tooling`] — recoverable
/// failures worth retrying rather than aborting the collab session. See
/// [`failure_class::classify`].
///
/// `CODEX_DISPATCH_FAILED_PREFIX` is deliberately in both this set and
/// `OFF_TURN_FAILURE_PREFIXES` above: it is both off-turn-admissible and
/// recoverable. The two prefix vocabularies overlap but are not identical —
/// `BRANCH_DRIFT_PREFIX` is off-turn-admissible but classifies as
/// `FailureClass::Terminal`, not `Tooling`.
pub const RECOVERABLE_FAILURE_PREFIXES: &[&str] = &[
    GIT_COMMIT_FAILED_PREFIX,
    GIT_PUSH_FAILED_PREFIX,
    SANDBOX_DENIED_PREFIX,
    DISK_FULL_PREFIX,
    NETWORK_FAILED_PREFIX,
    CODEX_DISPATCH_FAILED_PREFIX,
];
