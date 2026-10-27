//! End-to-end single-issue run loop (build-ladder rung 4).
//!
//! Rungs 1–3 built the pieces: storage ([`super::lineage`],
//! [`super::dispatch_state`], [`super::budget`]), the dispatch primitive
//! ([`super::dispatch`]) and its turn prompt ([`super::turn_prompt`]), and
//! the approved gate config ([`super::gate_config`], [`super::onboard`]).
//! Rung 2 stopped deliberately at "run one dispatch, parse its result" and
//! recorded the wiring as rung 4's job:
//!
//! > Wiring `DispatchOutcome` into rung 1's lineage/attempt-cap/budget-ledger
//! > write paths is explicitly NOT done — rung 2 stops at "run one dispatch,
//! > parse its result." Consuming that result (banking cost, deciding
//! > attempt-cap consumption, recording lineage) is rung 4's end-to-end job.
//!
//! This module is that consumer: [`run_issue`] drives one issue from an
//! approved gate config to a terminal state, dispatch by dispatch, and is
//! the first place in the ladder where the spec's *policy* — as opposed to
//! its storage shapes — actually executes.
//!
//! # What is deliberately NOT here
//!
//! The Lead's cross-issue responsibilities stay out: picking which issue to
//! work (`agent:ready` polling and `priority:*` ordering), the Reviewer
//! (rung 5), merge authority and GitHub label transitions (rung 6), and
//! process-/strategy-health supervision (rung 7). [`run_issue`] takes the
//! issue it is told to run and returns a [`TerminalReason`] for the caller
//! to act on — it never touches GitHub. Consequently the spec's "post a
//! comment summarizing everything tried, and flip the label to
//! `agent:exhausted`" is represented here by the *lineage* half only (the
//! terminal record), with the GitHub half left to rung 6.
//!
//! # The four ways a dispatch can land
//!
//! Everything this module decides flows from [`classify`], which turns one
//! [`DispatchOutcome`] into one of four cases. The distinction that matters
//! most is the last one, and it comes straight from the spec's error table:
//!
//! | Case | Consumes an attempt? | Why |
//! |---|---|---|
//! | [`Met`](DispatchClassification::Met) | yes | The gate is satisfied; terminal success. |
//! | [`Impossible`](DispatchClassification::Impossible) | yes | *"The goal clears and the invocation ends."* A real attempt that reached a conclusion. |
//! | [`FailedAttempt`](DispatchClassification::FailedAttempt) | yes | `not_met`, or a completed dispatch that produced no verdict — the spec's anti-stall case, which *"counts as an attempt"*. |
//! | [`InfrastructureFailure`](DispatchClassification::InfrastructureFailure) | **no** | *"Auth failure, exhausted credits, unrecoverable context overflow, or model unavailable ... these are infrastructure failures, never attempts, and must not consume the per-issue attempt cap."* |
//!
//! # Why infrastructure failures still need their own bound
//!
//! Exempting infrastructure failures from the attempt cap, as the spec
//! requires, removes the only thing that would otherwise stop the loop from
//! retrying them forever — and rung 2 measured a case that lands here and is
//! *cheap to reproduce but not free*: a dispatch cut off by `--max-turns`
//! returns `is_error: true` with **no `structured_output` at all**, even when
//! it did the work correctly. A misconfigured `max_turns` would therefore
//! spin indefinitely, banking real spend against the daily ledger on every
//! pass while never consuming a single attempt.
//!
//! Two guards, both new here:
//!
//! 1. [`RunConfig::validate`] refuses a `max_turns` that does not clear
//!    `n_turns` by [`MIN_TOOL_TURNS_PER_GOAL_TURN`] per goal turn — closing rung 2's flagged
//!    "N and `--max-turns` are coupled, not independent knobs" at the only
//!    layer that sees both numbers.
//! 2. [`RunConfig::max_consecutive_infrastructure_failures`] bounds the
//!    *consecutive* run of them. Consecutive, not total: a transient rate
//!    limit between two real attempts should not permanently consume the
//!    allowance, but a wedged configuration that can never produce a verdict
//!    must terminate.
//!
//! # Budget: pre-authorized, not observed after the fact
//!
//! The spec's ledger is *"the sum of `total_cost_usd` across IC and Reviewer
//! invocations, written to the daily ledger drawer as each dispatch
//! returns"*, and its error table stops dispatching once the daily budget is
//! exhausted. [`run_issue`] checks the ledger *before* each dispatch and
//! refuses to start one whose own hard `--max-budget-usd` ceiling could carry
//! the day past the cap. Because that ceiling is enforced by the CLI rather
//! than observed afterwards, the day's total is bounded by the cap by
//! construction instead of overshooting it by up to one dispatch.
//!
//! Cost is banked for **every** dispatch that returned a parseable result,
//! including failed and infrastructure-failed ones — the spec's error table
//! is explicit that a `--max-budget-usd` termination is *"treated as a failed
//! attempt"*, which presupposes its spend was still accounted for, and rung
//! 2 preserved `total_cost_usd` on a non-zero exit for exactly this caller.

use std::path::Path;

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::dispatch::{self, DispatchOutcome, DispatchSpec, SessionMode, SessionRefusal, Verdict};
use super::implementer::{Implementer, RESUME_REFUSALS_BEFORE_ROTATION};
use super::invocation::{InvocationArtifacts, InvocationRecovery};
use super::worktree::{self, Worktree};
use super::{
    blocked, budget, dispatch_state, gate_config, lineage, remediate, supervise, today_utc,
    turn_prompt, IssueRef,
};
use super::{AttemptOutcome, AttemptRecord, DispatchState, IssueStatus, PriorAttempt};

/// Turns per dispatch (the `N` in the spec's `/goal <gates> or stop after N
/// turns`). The spec's *IC lifecycle* section gives 5–8 as an explicitly
/// unmeasured starting suggestion and rung 0 declined to narrow it, because
/// its probe tasks were too light to observe the supervision-vs-cost tradeoff
/// that actually determines it. This is the midpoint of that range, not a
/// measurement.
pub const DEFAULT_N_TURNS: u32 = 6;

/// Floor on how many hard `--max-turns` each goal turn must be allowed.
///
/// **`N` and `--max-turns` are not the same unit.** `N` counts *goal* turns —
/// one evaluator round each — while the CLI's `--max-turns` counts every
/// tool-call round trip, of which a single goal turn spends many. Rung 2 read
/// its one data point (N=1 needing 5 turns against a `--max-turns 4`) as four
/// turns of fixed *overhead* and made the relation additive. That was the
/// right diagnosis in the wrong unit, and it held only because rung 2's probe
/// task was trivial enough for one goal turn to cost one tool call.
///
/// Measured on the first live run (2026-09-08, issue #285): the IC spent **64
/// tool calls in twelve minutes** merely reading code and tracing constructors
/// before its first edit, and completed the whole issue in 48. Against the
/// additive default that run produced — N=6 giving `--max-turns 12` — every
/// dispatch was killed roughly four minutes in, mid-edit, returning
/// `is_error: true` with no verdict; three of those in a row ended the run
/// without a single gate execution. Ten is a deliberately low floor: it
/// refuses the configuration now known to be broken without pretending to
/// know the true ratio, which varies with the task.
pub const MIN_TOOL_TURNS_PER_GOAL_TURN: u32 = 10;

/// Hard turns per goal turn [`RunConfig::new`] allows when the caller does not
/// set `max_turns` explicitly.
///
/// Generous on purpose. `--max-turns` is a runaway backstop, not an operating
/// bound: `--max-budget-usd` and the repo's wall-clock timeout are what should
/// actually stop a dispatch, because both are denominated in something the
/// operator set deliberately. A hard turn cap that binds first converts
/// completed work into an unrecorded failure — which is exactly what rung 2
/// measured, and what the first live run repeated at a larger scale. Fifty
/// clears the 48 that one measured real dispatch needed.
pub const DEFAULT_TOOL_TURNS_PER_GOAL_TURN: u32 = 50;

/// Per-issue attempt cap — the spec's *Cross-dispatch stagnation control*
/// counter, persisted in the issue-status drawer and therefore cumulative
/// across runs, not per-invocation. The spec mandates that a cap exist and
/// that reaching it is terminal; it names no number. Operator-tunable
/// placeholder.
pub const DEFAULT_ATTEMPT_CAP: u32 = 5;

/// Per-dispatch spend ceiling passed through to `--max-budget-usd`.
/// Operator-tunable placeholder; the spec names no figure. For scale, rung
/// 0's six validation probes cost $0.644 in total and rung 2's two
/// real-coding-task probes cost $0.398.
pub const DEFAULT_MAX_BUDGET_USD: f64 = 2.50;

/// Daily ledger ceiling across all dispatches. Operator-tunable placeholder;
/// the spec requires the ceiling to exist ("Daily token budget exhausted —
/// Lead stops dispatching and reports") without naming a figure.
pub const DEFAULT_DAILY_BUDGET_USD: f64 = 25.00;

/// How many *consecutive* infrastructure failures end the run. See the
/// module doc's *Why infrastructure failures still need their own bound*.
pub const DEFAULT_MAX_CONSECUTIVE_INFRASTRUCTURE_FAILURES: u32 = 3;

/// How many *unpriced* dispatches may run in one day.
///
/// The second ceiling, and rung 5's lesson carried over rather than
/// re-learned. A dispatch killed on the wall-clock bound really spent money,
/// but its result JSON — the only meter — died with it, so it banks to
/// `unpriced_dispatch_count`, which never moves `total_cost_usd`. The daily
/// dollar gate reads `total_cost_usd`. **A dollar ceiling therefore cannot
/// see this spend at all**, which is exactly the shape of rung 5's finding:
/// a ceiling denominated in units the thing being bounded never reports is
/// not a bound.
///
/// Five, not rung 5's twenty, because the units differ by an order of
/// magnitude: a timed-out dispatch is capped by `--max-budget-usd`
/// ($2.50 by default) where a review is a single Codex call. Five × $2.50 is
/// $12.50 of invisible worst case against a $25.00 default day — bounded,
/// and visibly less than the ceiling it cannot be seen by. Operator-tunable
/// placeholder like every other default here; the spec names no figure.
///
/// **The counter is not shared with [`super::review`]'s unpriced reviews.**
/// Reviews have their own `unpriced_review_count`, bounded by
/// `max_unpriced_reviews_per_day`. Sharing one counter let five reviews
/// exhaust this headroom and stop every IC dispatch — the same coupling
/// rung 9 removed for the advisor by giving it `unpriced_advice_count`.
/// This counter counts only timed-out IC dispatches, fix/remediation
/// runs included.
pub const DEFAULT_MAX_UNPRICED_DISPATCHES_PER_DAY: u32 = 5;

/// The issue's natural-language content, as the turn prompt needs it.
///
/// Supplied by the caller rather than fetched here: reading issues from
/// GitHub (and the `agent:ready` label traffic around it) is rung 6's
/// surface, and keeping this module free of network access is what lets the
/// whole loop be tested end to end against a real database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueBrief {
    pub title: String,
    pub body: String,
}

/// Policy knobs for one issue's run.
#[derive(Debug, Clone, PartialEq)]
pub struct RunConfig {
    pub implementer: Implementer,
    /// `--model` for the IC. The spec's *Model routing* table routes this by
    /// risk class (Sonnet, escalating to Opus); performing that routing is a
    /// later rung's job, so this module carries whatever the caller chose —
    /// exactly as [`DispatchSpec::model`] does.
    pub model: String,
    /// The Lead's dispatch-time risk class, recorded in the dispatch-state
    /// drawer so the Reviewer (rung 5) can later compare it against the
    /// class it derives from the actual diff. Never interpreted here.
    pub dispatch_class: String,
    pub n_turns: u32,
    pub max_turns: u32,
    pub max_budget_usd: f64,
    pub attempt_cap: u32,
    pub daily_budget_usd: f64,
    pub max_consecutive_infrastructure_failures: u32,
    /// See [`DEFAULT_MAX_UNPRICED_DISPATCHES_PER_DAY`]. The only bound that
    /// can see a timed-out dispatch's spend.
    pub max_unpriced_dispatches_per_day: u32,
}

/// Validate a model name against the selected implementation harness.
///
/// Model aliases are intentionally unrestricted, but their spelling is
/// preserved exactly in durable state and subprocess argv. Refusing
/// surrounding whitespace avoids validating one identity while dispatching
/// a subtly different one.
pub fn validate_implementation_model(
    implementer: Implementer,
    model: &str,
) -> Result<(), MemoryError> {
    let harness = implementer.label();
    if model.trim().is_empty() {
        return Err(MemoryError::Validation(format!(
            "model must not be empty for the {harness} implementer"
        )));
    }
    if model.trim() != model {
        return Err(MemoryError::Validation(format!(
            "model must not contain leading or trailing whitespace for the {harness} implementer"
        )));
    }
    // Refused here rather than first by `MuseCli::argv`, which keeps the same
    // check: by then the worktree, branch and invocation directory exist.
    if implementer == Implementer::Muse && !super::muse_dispatch::is_marker_safe(model) {
        return Err(MemoryError::Validation(format!(
            "model {model:?} cannot be passed to the Muse implementer: it must contain no \
             whitespace and not start with '-'"
        )));
    }
    let foreign = match implementer {
        Implementer::Claude => model.starts_with("gpt-") || model.starts_with("muse-"),
        Implementer::Codex => model.starts_with("claude-") || model.starts_with("muse-"),
        Implementer::Muse => model.starts_with("claude-") || model.starts_with("gpt-"),
    };
    if foreign {
        return Err(MemoryError::Validation(format!(
            "model '{model}' is incompatible with the {harness} implementer; choose a {harness} model"
        )));
    }
    Ok(())
}

/// Refuse a worktree root the selected harness cannot dispatch under, before
/// anything is provisioned beneath it.
///
/// Muse passes the issue worktree as `--workspace`, one word of the
/// process-table marker grammar, so a root with whitespace would provision a
/// branch and worktree that every dispatch then refuses. The worktree path is
/// persisted resolved, so the resolved spelling is checked too.
pub fn validate_worktree_root(implementer: Implementer, root: &Path) -> Result<(), MemoryError> {
    let cwd = std::env::current_dir()?;
    validate_worktree_root_from(implementer, root, &cwd)
}

/// [`validate_worktree_root`] with a relative `root` read against `cwd`.
///
/// A relative root that does not exist yet resolves to itself, so neither
/// spelling would carry the working directory's whitespace; the worktree is
/// still created beneath it. It is made absolute first.
fn validate_worktree_root_from(
    implementer: Implementer,
    root: &Path,
    cwd: &Path,
) -> Result<(), MemoryError> {
    if implementer != Implementer::Muse {
        return Ok(());
    }
    let absolute = cwd.join(root);
    for path in [absolute.clone(), worktree::canonical_or_self(&absolute)] {
        let text = path.to_str().ok_or_else(|| {
            MemoryError::Validation(format!(
                "unsupported worktree root for the Muse implementer: {} is not UTF-8",
                path.display()
            ))
        })?;
        if text.chars().any(char::is_whitespace) {
            return Err(MemoryError::Validation(format!(
                "unsupported worktree root for the Muse implementer: {text:?} contains \
                 whitespace; choose a --worktree-root without whitespace, because Muse's \
                 --workspace is one word of the argv that process identity recovery parses"
            )));
        }
    }
    Ok(())
}

impl RunConfig {
    /// A config with every documented default, for the two values that have
    /// no sensible default: the model and the dispatch-time risk class.
    pub fn new(model: impl Into<String>, dispatch_class: impl Into<String>) -> Self {
        Self {
            implementer: Implementer::Claude,
            model: model.into(),
            dispatch_class: dispatch_class.into(),
            n_turns: DEFAULT_N_TURNS,
            max_turns: DEFAULT_N_TURNS.saturating_mul(DEFAULT_TOOL_TURNS_PER_GOAL_TURN),
            max_budget_usd: DEFAULT_MAX_BUDGET_USD,
            attempt_cap: DEFAULT_ATTEMPT_CAP,
            daily_budget_usd: DEFAULT_DAILY_BUDGET_USD,
            max_consecutive_infrastructure_failures:
                DEFAULT_MAX_CONSECUTIVE_INFRASTRUCTURE_FAILURES,
            max_unpriced_dispatches_per_day: DEFAULT_MAX_UNPRICED_DISPATCHES_PER_DAY,
        }
    }

    /// Reject a configuration that cannot produce a usable dispatch.
    ///
    /// The `max_turns`/`n_turns` headroom check is the load-bearing one; see
    /// [`MIN_TOOL_TURNS_PER_GOAL_TURN`]. The budget check is the other: a
    /// `max_budget_usd` above `daily_budget_usd` means the pre-authorization
    /// in [`run_issue`] can never clear, so the run would report a budget
    /// stop without ever dispatching — a misconfiguration worth naming at
    /// the point it is made rather than diagnosing from a silent no-op.
    pub fn validate(&self) -> Result<(), MemoryError> {
        validate_implementation_model(self.implementer, &self.model)?;
        if self.dispatch_class.trim().is_empty() {
            return Err(MemoryError::Validation(
                "dispatch_class must not be empty".into(),
            ));
        }
        if self.n_turns == 0 {
            return Err(MemoryError::Validation("n_turns must be at least 1".into()));
        }
        if self.attempt_cap == 0 {
            return Err(MemoryError::Validation(
                "attempt_cap must be at least 1".into(),
            ));
        }
        if self.max_consecutive_infrastructure_failures == 0 {
            return Err(MemoryError::Validation(
                "max_consecutive_infrastructure_failures must be at least 1".into(),
            ));
        }
        if self.max_unpriced_dispatches_per_day == 0 {
            return Err(MemoryError::Validation(
                "max_unpriced_dispatches_per_day must be at least 1 — zero would refuse every \
                 dispatch as soon as one timed out, on a repo with a wall-clock bound"
                    .into(),
            ));
        }
        let turn_floor = self.n_turns.saturating_mul(MIN_TOOL_TURNS_PER_GOAL_TURN);
        if self.max_turns < turn_floor {
            return Err(MemoryError::Validation(format!(
                "max_turns ({}) must be at least {} — {} goal turns at {} tool-call turns each. \
                 The two count different things, and a hard --max-turns cap that fires before \
                 the goal loop's own stop suppresses the verdict schema entirely, so a completed \
                 dispatch returns as an unrecorded failure",
                self.max_turns, turn_floor, self.n_turns, MIN_TOOL_TURNS_PER_GOAL_TURN
            )));
        }
        if !self.max_budget_usd.is_finite() || self.max_budget_usd <= 0.0 {
            return Err(MemoryError::Validation(
                "max_budget_usd must be a finite, positive number".into(),
            ));
        }
        if !self.daily_budget_usd.is_finite() || self.daily_budget_usd <= 0.0 {
            return Err(MemoryError::Validation(
                "daily_budget_usd must be a finite, positive number".into(),
            ));
        }
        if self.max_budget_usd > self.daily_budget_usd {
            // A supervised CLI has no --max-budget-usd, so naming it would send
            // the operator after a flag the CLI refuses for this implementer.
            return Err(MemoryError::Validation(
                if self.implementer.recovers_invocations() {
                    format!(
                        "daily_budget_usd ({}) is below the ${} every {} dispatch reserves \
                     before it starts — {} reports no cost, so each invocation is \
                     pre-authorized at that fixed ceiling. Raise --daily-budget-usd to at \
                     least {}",
                        self.daily_budget_usd,
                        self.max_budget_usd,
                        self.implementer.label(),
                        self.implementer.label(),
                        self.max_budget_usd
                    )
                } else {
                    format!(
                        "max_budget_usd ({}) exceeds daily_budget_usd ({}) — no dispatch could \
                     ever be authorized",
                        self.max_budget_usd, self.daily_budget_usd
                    )
                },
            ));
        }
        Ok(())
    }
}

/// What one dispatch's outcome means for the run. See the module doc's
/// *four ways a dispatch can land* table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DispatchClassification {
    Met,
    Impossible,
    FailedAttempt,
    InfrastructureFailure,
}

impl DispatchClassification {
    /// Whether this outcome consumes one of the issue's bounded attempts.
    pub fn consumes_attempt(self) -> bool {
        !matches!(self, DispatchClassification::InfrastructureFailure)
    }
}

/// Classify one dispatch outcome.
///
/// Order is deliberate and the guard clauses are not interchangeable:
///
/// - `is_met()` first, because it is the only case that already folds in
///   every trust check ([`DispatchOutcome::process_success`], `!is_error`,
///   and the schema-forced verdict) in one place — rung 2's 6a guard.
/// - Infrastructure **before** `is_impossible()`, so a process that died
///   before exiting cleanly cannot have its verdict believed. Rung 2's own
///   doc for `process_success` is explicit that a process can flush a
///   complete, schema-valid result and then die for an unrelated reason;
///   trusting an `impossible` from such a process would burn a real attempt
///   on an infrastructure fault, which the spec forbids.
/// - A completed dispatch with **no** verdict is a `FailedAttempt`, not an
///   infrastructure failure: that is the spec's anti-stall row ("Returns
///   control with the goal still set ... it counts as an attempt"). Only an
///   *errored* dispatch with no verdict is infrastructure.
/// - ...**unless it did nothing at all** — no turns and no spend. That floor
///   is the last guard before the anti-stall fallthrough, and it must stay
///   below the `timed_out` arm, whose own `0.0` cost means *unknown* rather
///   than *free* and would otherwise be caught here for the wrong reason.
pub fn classify(outcome: &DispatchOutcome) -> DispatchClassification {
    if outcome.is_met() {
        return DispatchClassification::Met;
    }
    // A wall-clock kill, stated explicitly rather than left to fall through
    // the `!process_success` arm below it would already hit. Two reasons to
    // spell it out: it documents the decision, and it stops a later change to
    // how `process_success` is set from silently reclassifying it.
    //
    // Infrastructure, not a failed attempt — despite the spec calling a
    // `--max-budget-usd` termination a failed attempt, which looks like the
    // same thing. It is not: a spend-terminated dispatch still returns its
    // result JSON, so there is a real diagnostic to record as `why_failed`
    // and a real meter to bank. A killed one returns nothing at all, and an
    // attempt record whose only content is "we killed it" would consume one
    // of the issue's five attempts while adding nothing the next dispatch can
    // learn from. Whose problem it is settles it too: repeated timeouts mean
    // the bound is wrong or the repo is wedged — an operator's fix, which is
    // exactly what the consecutive-infrastructure-failure bound escalates to.
    if outcome.timed_out {
        return DispatchClassification::InfrastructureFailure;
    }
    if !outcome.process_success || (outcome.is_error && outcome.verdict.is_none()) {
        return DispatchClassification::InfrastructureFailure;
    }
    if outcome.is_impossible() {
        return DispatchClassification::Impossible;
    }
    // The anti-stall rule's floor, and the last guard before it. A dispatch
    // that produced **no turns and no spend** did not "return control with
    // the goal still set" — it never took the goal up at all, and an attempt
    // record whose only content is "nothing happened" consumes one of the
    // issue's five attempts while adding nothing the next dispatch can learn
    // from. That is the same argument the `timed_out` arm above makes for its
    // own case, and it lands on the same class for the same reason.
    //
    // Measured on the first live run (2026-09-08, issue #285): a `--resume`
    // into a session whose `/goal` loop had **already concluded** exchanged
    // one "continue" prompt for one "no response requested" and exited
    // cleanly in ~6 seconds — `num_turns: 0`, `total_cost_usd: 0.0`, no
    // verdict, `is_error: false`, exit zero. Read as a `FailedAttempt` it
    // burned four of the issue's five attempts in 25 seconds and drove the
    // run straight to `AttemptCapExhausted`. Without this floor, *any* resume
    // that no-ops destroys an issue's whole attempt budget in under half a
    // minute.
    //
    // **Both readings are required**, because either alone is ambiguous. A
    // `0.0` cost can mean *unknown* rather than *free* — that is exactly what
    // it means for a killed dispatch, whose arm sits deliberately above this
    // one so it never reaches here (see `DispatchOutcome::timed_out`). And a
    // 0-turn dispatch that was nonetheless billed did engage the model, so it
    // is a real, if short, attempt.
    //
    // Whose problem it is settles the class, exactly as it does for a
    // timeout: a resume that no-ops is a wedged session rather than a failing
    // approach, so it is an operator's fix, and
    // `max_consecutive_infrastructure_failures` is what escalates to them
    // instead of the attempt cap silently retiring the issue.
    // An absent meter reads like the old `0.0` default here: a result that
    // reported neither a turn nor a price took nothing up either.
    if outcome.verdict.is_none()
        && outcome.num_turns == 0
        && outcome.cost_usd.is_none_or(|cost| cost == 0.0)
    {
        return DispatchClassification::InfrastructureFailure;
    }
    DispatchClassification::FailedAttempt
}

/// Why the run stopped.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "terminal", rename_all = "snake_case")]
pub enum TerminalReason {
    /// An existing implementation may still be running; no new invocation is authorized.
    ImplementationInFlight { detail: String },
    /// The gate condition was satisfied. Terminal for the issue: the
    /// dispatch-state drawer is cleared and the branch is ready for the
    /// Reviewer (rung 5).
    Met { commit_sha: Option<String> },
    /// A prior run already recorded a success for this issue, so nothing was
    /// dispatched. Makes [`run_issue`] idempotent — re-running a finished
    /// issue costs nothing rather than re-doing settled work.
    ///
    /// ⟨rung 11⟩ Not returned when a remediation is in force
    /// ([`super::remediate::active_remediation`]): a reviewer that asked for
    /// changes is the spec's own reason to re-dispatch a succeeded issue, and
    /// the attempt cap below is what bounds it.
    AlreadySucceeded { commit_sha: Option<String> },
    /// The per-issue attempt cap was reached — for a remediation, its own cap
    /// counted from the issue's first success. Terminal: per the spec,
    /// `agent:exhausted` *"never self-resumes"*, so the dispatch state is
    /// cleared and only a human `autopilot retry` brings it back (a re-label
    /// alone leaves the cap in force).
    AttemptCapExhausted,
    /// The goal evaluator judged the condition unsatisfiable.
    Impossible { reason: Option<String> },
    /// The daily ledger cannot authorize another dispatch. **Not** terminal
    /// for the issue: the dispatch-state drawer is left in place so the same
    /// session resumes once the ledger rolls over.
    DailyBudgetExhausted { spent_usd: f64 },
    /// Consecutive infrastructure failures hit their bound. Also non-terminal
    /// for the issue — the state is kept so a restart can adopt it — but it
    /// needs human attention rather than another automatic pass.
    InfrastructureFailure { consecutive: u32 },
    /// The day's *unpriced* dispatch ceiling is reached.
    ///
    /// Exists because the dollar ceiling cannot: a timed-out dispatch's spend
    /// never reaches `total_cost_usd`. "Paused, not finished" — the ledger
    /// rolls over at midnight, so the session is kept.
    UnpricedDispatchesExhausted { unpriced_today: u32 },
    /// Rung 7's strategy-health escalated this issue: a dispatch already ran
    /// with a redirect in force and failed the same way regardless.
    ///
    /// "Paused, not finished", like the two above — the dispatch state is
    /// kept, because a human who clears the escalation should resume the
    /// session rather than start over. It never self-resumes; only
    /// `ironmem autopilot supervise --clear-escalation` retries it, exactly
    /// as `agent:exhausted` requires a human re-label.
    StrategyEscalated { signature: String },
    /// The harness reported its session still in use after an invocation
    /// finished: something still holds it. A retry can never succeed and a
    /// fresh session could start a second implementer over the holder, so
    /// the run stops on the first one and the state is parked as
    /// `paused-session-in-use`, which the queue defers until an operator
    /// clears it. `detail` says how.
    SessionInUse { detail: String },
}

impl TerminalReason {
    /// Whether the issue's dispatch-state drawer should be cleared. False for
    /// the two "paused, not finished" reasons — clearing those would turn a
    /// resumable session into an orphan, which the spec's restart table
    /// requires be flagged for a human rather than silently adopted.
    fn clears_dispatch_state(&self) -> bool {
        !matches!(
            self,
            TerminalReason::ImplementationInFlight { .. }
                | TerminalReason::DailyBudgetExhausted { .. }
                | TerminalReason::InfrastructureFailure { .. }
                | TerminalReason::StrategyEscalated { .. }
                | TerminalReason::UnpricedDispatchesExhausted { .. }
                | TerminalReason::SessionInUse { .. }
        )
    }
}

/// One dispatch's contribution to the run, for the caller's report.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DispatchSummary {
    /// The issue's cumulative attempt number this dispatch was recorded
    /// under, or `None` for an infrastructure failure (which records no
    /// lineage attempt at all).
    pub attempt_n: Option<u32>,
    pub classification: DispatchClassification,
    pub total_cost_usd: Option<f64>,
    pub num_turns: u32,
    pub verdict: Option<Verdict>,
    pub reason: Option<String>,
}

/// The result of driving one issue.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IssueRun {
    /// Unknown-price outcomes or reservations observed by this run, including
    /// a held invocation reserved on an earlier day. Observing a reservation
    /// or recovering an outcome never bills it again.
    pub unpriced_dispatches: u32,
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub terminal: TerminalReason,
    pub dispatches: Vec<DispatchSummary>,
    /// Known dollars this run added to the ledger. With unknown-price
    /// invocations this is a floor; see `unpriced_dispatches`.
    pub total_cost_usd: f64,
    /// The issue's cumulative attempt count after the run.
    pub cumulative_attempt_n: u32,
    /// Effective per-dispatch wall-clock bound. Supervised CLI harnesses (Codex,
    /// Muse) use the finite backend default when the repo has none; Claude retains its optional repo bound.
    pub wall_clock_timeout_secs: Option<u64>,
}

/// How [`run_issue`] executes a dispatch.
///
/// A trait rather than a direct call to [`dispatch::run_dispatch`] so the
/// whole loop — attempt-cap arithmetic, budget pre-authorization, lineage
/// and dispatch-state writes, terminal classification — is testable against
/// a real database without spawning `claude` and spending real money. The
/// production implementation is [`ClaudeDispatcher`].
///
/// # Error contract
///
/// An implementation must report a failure to **start** the process as
/// [`MemoryError::NotFound`], and anything else as a different variant.
/// [`run_issue`] reads that distinction to decide whether the next
/// invocation may still use `--session-id`: a process that never launched
/// created no session, whereas one that ran and then produced no parseable
/// result probably did, and re-supplying `--session-id` for a uuid that
/// already exists fails. [`dispatch::run_dispatch`] follows this — it returns
/// `NotFound` only from the `Command::output()` call itself.
pub trait Dispatcher {
    fn implementer(&self) -> Implementer {
        Implementer::Claude
    }

    fn recover(
        &mut self,
        _repo: &Path,
        _state: &DispatchState,
    ) -> Result<InvocationRecovery, MemoryError> {
        Ok(InvocationRecovery::NoPriorInvocation)
    }

    fn dispatch(
        &mut self,
        repo: &Path,
        spec: &DispatchSpec,
    ) -> Result<DispatchOutcome, MemoryError>;
}

/// The real dispatcher: runs `claude` per [`dispatch::run_dispatch`].
pub struct ClaudeDispatcher {
    bin: std::path::PathBuf,
}

impl ClaudeDispatcher {
    /// Resolve the `claude` binary once, up front, so a missing binary fails
    /// before any state is written rather than midway through a run.
    pub fn resolve() -> Result<Self, MemoryError> {
        Ok(Self {
            bin: dispatch::resolve_claude_binary()?,
        })
    }
}

impl Dispatcher for ClaudeDispatcher {
    fn dispatch(
        &mut self,
        repo: &Path,
        spec: &DispatchSpec,
    ) -> Result<DispatchOutcome, MemoryError> {
        dispatch::run_dispatch(&self.bin, repo, spec)
    }
}

/// A supervised CLI implementation backend (Codex, Muse). Process evidence is
/// checked before durable artifacts; only trusted supervisor cleanup permits
/// completed recovery.
pub struct CliDispatcher {
    implementer: Implementer,
    bin: std::path::PathBuf,
    /// The socket the operator's MCP proxies connect to, and so where one
    /// autospawns the shared daemon (`harness::proxy_command_args` registers
    /// `serve --connect` with this same `Config::daemon_socket_path`), with
    /// the controller's own canonical executable, the binary that daemon
    /// runs. `None` when that executable cannot be resolved: nothing is
    /// then exempt from cleanup.
    shared_daemon: Option<super::process_tree::SharedDaemon>,
}

impl CliDispatcher {
    pub fn resolve(implementer: Implementer) -> Result<Self, MemoryError> {
        super::invocation::cli_for(implementer)?;
        let path_var = std::env::var_os("PATH").unwrap_or_default();
        Ok(Self {
            implementer,
            bin: find_launcher(implementer.launcher_harness().binary(), &path_var)?,
            // The socket derives from the state directory and
            // `IRONMEM_DAEMON_SOCKET`, never from a `--db` override.
            shared_daemon: super::process_tree::SharedDaemon::for_current_exe(
                shared_daemon_socket(crate::config::Config::load(None))?,
            ),
        })
    }

    /// Refuse work for another harness: `bin` is this dispatcher's binary,
    /// and handing it another harness's argv or artifacts would run one CLI
    /// as if it were the other.
    fn check_harness(&self, other: Implementer) -> Result<(), MemoryError> {
        if other == self.implementer {
            return Ok(());
        }
        Err(MemoryError::Validation(format!(
            "implementation harness mismatch: the {} dispatcher was handed a {} invocation",
            self.implementer.label(),
            other.label()
        )))
    }
}

/// `name` on `path_var`, skipping relative entries. A supervised harness is
/// spawned in the issue worktree, where a relative entry such as `.` would
/// resolve to a file in the repository being worked on.
fn find_launcher(
    name: &str,
    path_var: &std::ffi::OsStr,
) -> Result<std::path::PathBuf, MemoryError> {
    let absolute = std::env::split_paths(path_var).filter(|dir| dir.is_absolute());
    let path_var = std::env::join_paths(absolute)
        .map_err(|e| MemoryError::Validation(format!("unusable PATH: {e}")))?;
    crate::launcher::find_in_paths(name, &path_var)
}

/// The shared daemon's socket from a loaded config. Loading it parses every
/// `IRONMEM_*` setting, so a failure here names what it was loaded for.
fn shared_daemon_socket(
    config: Result<crate::config::Config, MemoryError>,
) -> Result<std::path::PathBuf, MemoryError> {
    config
        .map(|config| config.daemon_socket_path())
        .map_err(|e| {
            MemoryError::Validation(format!(
                "cannot resolve the shared ironmem daemon socket, which supervised \
                 implementer cleanup must spare, because the ironmem config failed to load: {e}"
            ))
        })
}

impl Dispatcher for CliDispatcher {
    fn implementer(&self) -> Implementer {
        self.implementer
    }

    fn recover(
        &mut self,
        _repo: &Path,
        state: &DispatchState,
    ) -> Result<InvocationRecovery, MemoryError> {
        self.check_harness(state.implementer)?;
        let snapshot = super::process_tree::snapshot(&mut super::process_tree::UnixProcesses);
        match super::registry::invocation_state_liveness(&snapshot, state) {
            super::registry::Liveness::Alive => Ok(InvocationRecovery::Alive),
            super::registry::Liveness::NotListed => match state.invocation_artifacts.as_ref() {
                Some(artifacts) => {
                    super::invocation::recover_artifacts(self.implementer, artifacts)
                }
                None => Ok(InvocationRecovery::NoPriorInvocation),
            },
            super::registry::Liveness::Unknown => Ok(InvocationRecovery::Unknown {
                reason: super::registry::invocation_state_reason(&snapshot, state),
            }),
        }
    }

    fn dispatch(
        &mut self,
        repo: &Path,
        spec: &DispatchSpec,
    ) -> Result<DispatchOutcome, MemoryError> {
        self.check_harness(spec.implementer)?;
        super::invocation::run_supervised(&self.bin, repo, spec, self.shared_daemon.as_ref())
    }
}

/// `IssueRef` is a plain domain type with no wire shape of its own — each
/// storage kind serializes it differently (see [`super::dispatch_state`]'s
/// note on `issue`/`repo` as siblings versus [`super::lineage`]'s canonical
/// `repo#number` string). A run report is a third consumer, so it spells the
/// issue out here rather than putting a `Serialize` impl on `IssueRef` that
/// would silently become a fourth, unversioned wire format.
fn serialize_issue<S: serde::Serializer>(
    issue: &IssueRef,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::ser::SerializeStruct;
    let mut state = serializer.serialize_struct("issue", 3)?;
    state.serialize_field("repo", &issue.repo)?;
    state.serialize_field("number", &issue.number)?;
    state.serialize_field("canonical", &issue.canonical())?;
    state.end()
}

/// The commit the issue branch (`refs/heads/<branch>`) points at, for the
/// lineage record's `commit_sha`. It is the branch tip, not `HEAD`, because
/// the PR is opened from the branch and the IC may have left `HEAD` on
/// another checkout; it is the same tip the signing guard checked. Best-effort: an IC
/// that satisfied a read-only gate without committing anything is a
/// legitimate success, so an unreadable branch degrades to `None` rather than
/// failing a run that otherwise succeeded.
fn branch_tip(worktree: &Worktree) -> Option<String> {
    let mut command = std::process::Command::new("git");
    // An inherited `GIT_DIR` overrides `current_dir`, so without this the
    // commit recorded as an attempt's `commit_sha` could be another
    // repository's — a wrong answer that looks exactly like a right one.
    crate::mcp::tools::scrub_git_environment(&mut command);
    let output = command
        .args([
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{}^{{commit}}", worktree.branch),
        ])
        .current_dir(&worktree.path)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// How much of the model's own reason a `met` the guard refused keeps.
const MODEL_REASON_CHARS: usize = 500;

/// How many hex digits name a commit in a violation.
const SHORT_SHA_CHARS: usize = 12;

/// How long one signing-guard check may run, all its git commands together,
/// before the running one is killed with its process group and the guard
/// fails closed. One deadline for the whole check, so neither a long range
/// nor the one-commit-at-a-time fallback can stretch it. Generous: a range of
/// thousands of commits reads in well under a second.
const SIGNING_GUARD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// How often [`GuardGit::run`] checks on its git process.
const GUARD_GIT_POLL: std::time::Duration = std::time::Duration::from_millis(10);

/// The first `n` characters of `text`.
fn prefix(text: &str, n: usize) -> String {
    text.chars().take(n).collect()
}

/// What may hold a branch to the signing policy beyond its recorded flags:
/// the policy as this run, or the interrupted run whose dispatch state it
/// took over, read it at its start. A value rather than a bare `bool`, so each
/// source names itself in the violation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct SigningPolicySeen {
    /// `commit.gpgsign` read `true` when this run started.
    at_run_start: bool,
    /// The dispatch state this run started from records that the run that
    /// wrote it saw it on ([`DispatchState::signing_held`]). That memory
    /// survives a controller crash, where the run's own does not, so it
    /// holds only the `met` of the invocation that run launched, recovered
    /// as finished: never a fresh dispatch, and never a branch an operator
    /// opted out.
    by_interrupted_run: bool,
}

impl SigningPolicySeen {
    /// What holds a fresh dispatch's `met`: this run's own reading only.
    fn fresh(self) -> Self {
        Self {
            by_interrupted_run: false,
            ..self
        }
    }

    /// The source that holds the branch, named for a reason, if any does.
    fn holder(self) -> Option<&'static str> {
        if self.at_run_start {
            Some("the policy, which was on when this run started")
        } else if self.by_interrupted_run {
            Some("the policy, which was on when an interrupted run on this issue started")
        } else {
            None
        }
    }
}

/// Why a `met` breaks the signing policy: the model's to fix, so the `met`
/// becomes a failed attempt whose reason reaches the next turn.
#[derive(Debug, Clone, PartialEq, Eq)]
enum SigningViolation {
    /// `commit.gpgsign` was turned off while `holder` still holds the branch.
    PolicyDisabled { holder: String },
    /// The issue branch is gone, so nothing that ships can be checked.
    MissingBranch { branch: String },
    /// Commits on the branch are unsigned or carry a bad signature: short
    /// shas, newest first, and the remote the recipe pushes to.
    Unsigned {
        flagged: Vec<String>,
        cut: String,
        branch: String,
        remote: String,
    },
}

impl std::fmt::Display for SigningViolation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SigningViolation::PolicyDisabled { holder } => write!(
                f,
                "commit.gpgsign was turned off, but this branch is still held to signing by \
                 {holder}. Restore it with `git config --unset commit.gpgsign` (or set it back \
                 to true) and sign every commit. If signing fails in your environment, return \
                 verdict impossible with the signing error; never bypass."
            ),
            SigningViolation::MissingBranch { branch } => write!(
                f,
                "commit.gpgsign is on, but the issue branch `{branch}` does not exist, so the \
                 commits that ship cannot be checked. Recreate it at your work (`git checkout -B \
                 {branch} <your last commit>`), with every commit signed."
            ),
            SigningViolation::Unsigned {
                flagged,
                cut,
                branch,
                remote,
            } => f.write_str(&unsigned_violation(flagged, cut, branch, remote)),
        }
    }
}

/// Why the guard could not judge a `met`. Never the model's fault, so never
/// an attempt; the `met` is still never accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
enum GuardFault {
    /// `commit.gpgsign`, or a flag recording it, cannot be read: an
    /// operator's config fault that every later `met` would meet too.
    PolicyUnreadable(String),
    /// Unsigned commits are on the branch, but none was committed under this
    /// run's identity: a collaborator's push the model is not to rewrite.
    ForeignUnsigned {
        commits: Vec<(String, String)>,
        identity: String,
    },
    /// git could not be run, or could not read the branch.
    GitUnreadable(String),
}

impl GuardFault {
    /// Whether every later dispatch's `met` would meet this fault too, so
    /// the run stops before paying for another one.
    fn stops_run(&self) -> bool {
        !matches!(self, GuardFault::GitUnreadable(_))
    }
}

impl std::fmt::Display for GuardFault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardFault::PolicyUnreadable(why) => f.write_str(why),
            GuardFault::ForeignUnsigned { commits, identity } => {
                let named = commits
                    .iter()
                    .take(NAMED_UNSIGNED_COMMITS)
                    .map(|(sha, committer)| format!("{sha} (committer {committer})"))
                    .collect::<Vec<_>>()
                    .join(", ");
                let more = commits.len().saturating_sub(NAMED_UNSIGNED_COMMITS);
                let more = if more > 0 {
                    format!(" and {more} more")
                } else {
                    String::new()
                };
                write!(
                    f,
                    "commit.gpgsign is on, but commit(s) {named}{more} on this branch are \
                     unsigned or carry a bad signature, and none was committed as this run's \
                     identity: the commits name their committers above, and this run commits \
                     as {identity}. So the model is not charged for them. If those are the \
                     same person, the implementer committed under another identity. Re-sign \
                     or drop them and push the branch, or opt it out of signing (see Commit \
                     signing in docs/AUTOPILOT.md)"
                )
            }
            GuardFault::GitUnreadable(why) => write!(
                f,
                "commit.gpgsign is on, but the branch's commit signatures could not be read: \
                 {why}"
            ),
        }
    }
}

/// The signing guard's git: every command bounded in time, with the
/// signature programs pinned to the operator's own.
///
/// `%G?` makes git run `gpg.program`, `gpg.ssh.program` or
/// `gpg.x509.program` here, in the controller, after the invocation's
/// supervision has ended. The repository's `.git/config` is writable by the
/// unconfined agent, so a program set there would run outside any bound. Each
/// is pinned with `-c` to the operator's global or system value (git's
/// default when neither sets one), and every command, with its process group,
/// is killed after the timeout, failing the guard closed. Messages are read
/// in the C locale, because one of them is matched.
struct GuardGit<'a> {
    repo: &'a Path,
    timeout: std::time::Duration,
    /// When the whole check must be done, shared by every command.
    deadline: std::time::Instant,
    pins: Vec<String>,
    /// Variables removed from every command's environment (tests only).
    unset_env: &'static [&'static str],
}

impl<'a> GuardGit<'a> {
    fn new(repo: &'a Path, timeout: std::time::Duration) -> Result<Self, String> {
        Self::build(repo, timeout, &[])
    }

    fn build(
        repo: &'a Path,
        timeout: std::time::Duration,
        unset_env: &'static [&'static str],
    ) -> Result<Self, String> {
        let mut git = Self {
            repo,
            timeout,
            deadline: std::time::Instant::now() + timeout,
            pins: Vec::new(),
            unset_env,
        };
        let (mut openpgp, mut ssh, mut x509) = ("gpg", "ssh-keygen", "gpgsm");
        let mut read = Vec::new();
        // System, then global: the order git reads them, so a later value wins.
        for scope in ["--system", "--global"] {
            let out = git.run(
                &[
                    "config",
                    scope,
                    "--includes",
                    "--get-regexp",
                    r"^gpg\..*program$",
                ],
                None,
            )?;
            match out.status.code() {
                Some(0) => read.push(String::from_utf8_lossy(&out.stdout).into_owned()),
                Some(1) => {} // none set in that scope
                // Unreadable (`$HOME` unset under launchd exits 128): git's
                // own defaults stand for that scope, never the repository's.
                _ => tracing::warn!(
                    scope,
                    error = %worktree::bounded_stderr(&out.stderr),
                    "autopilot's commit-signing guard cannot read the operator's signature \
                     programs in this scope; git's defaults are used for it"
                ),
            }
        }
        for line in read.iter().flat_map(|text| text.lines()) {
            let Some((key, value)) = line.split_once(' ') else {
                continue;
            };
            match key {
                "gpg.program" | "gpg.openpgp.program" => openpgp = value,
                "gpg.ssh.program" => ssh = value,
                "gpg.x509.program" => x509 = value,
                _ => {}
            }
        }
        git.pins = vec![
            format!("gpg.program={openpgp}"),
            format!("gpg.openpgp.program={openpgp}"),
            format!("gpg.ssh.program={ssh}"),
            format!("gpg.x509.program={x509}"),
        ];
        Ok(git)
    }

    /// Run git with `args`, writing `input` one line each to its stdin.
    fn run(&self, args: &[&str], input: Option<&[&str]>) -> Result<std::process::Output, String> {
        use std::process::Stdio;
        let label = || format!("git {}", args.join(" "));
        let mut command = std::process::Command::new("git");
        crate::mcp::tools::scrub_git_environment(&mut command);
        // No fetch of any kind: a missing object in a partial clone would
        // otherwise be fetched through the repository's own transport config.
        command
            .env("LC_ALL", "C")
            .env("LANG", "C")
            .env("GIT_NO_LAZY_FETCH", "1");
        for key in self.unset_env {
            command.env_remove(key);
        }
        command.args(["-c", "protocol.allow=never"]);
        for pin in &self.pins {
            command.arg("-c").arg(pin);
        }
        command
            .args(args)
            .current_dir(self.repo)
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
        }
        let mut child = dispatch::retry_on_etxtbsy(|| command.spawn())
            .map_err(|e| format!("{} could not run: {e}", label()))?;
        // Written from another thread: git answers as it reads, so a writer
        // that filled git's input before reading its output would deadlock on
        // a long range.
        let writer = match (input, child.stdin.take()) {
            (Some(lines), Some(mut stdin)) => {
                let text: String = lines.iter().map(|line| format!("{line}\n")).collect();
                Some(std::thread::spawn(move || {
                    std::io::Write::write_all(&mut stdin, text.as_bytes())
                }))
            }
            _ => None,
        };
        let stdout = dispatch::drain(child.stdout.take());
        let stderr = dispatch::drain(child.stderr.take());
        let deadline = self.deadline;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(GUARD_GIT_POLL)
                }
                waited => {
                    dispatch::kill_process_group(&mut child);
                    let _ = child.wait();
                    return Err(match waited {
                        Err(e) => format!("{} could not be waited on: {e}", label()),
                        _ => format!(
                            "{} did not finish within the guard's {}s and was killed",
                            label(),
                            self.timeout.as_secs_f64()
                        ),
                    });
                }
            }
        };
        // A descendant git left behind can hold a pipe open. The readers get
        // what is left of the same deadline and are then abandoned.
        let open = !joins_by(&stdout, deadline)
            || !joins_by(&stderr, deadline)
            || writer.as_ref().is_some_and(|w| !joins_by(w, deadline));
        if open {
            // Whatever holds the pipe is still in the group git led: a group
            // id is not reused while any member lives.
            dispatch::kill_process_group(&mut child);
            return Err(format!(
                "{} exited but left its output open past {}s",
                label(),
                self.timeout.as_secs_f64()
            ));
        }
        let output = std::process::Output {
            status,
            stdout: stdout.join().unwrap_or_default(),
            stderr: stderr.join().unwrap_or_default(),
        };
        match writer.map(std::thread::JoinHandle::join) {
            None | Some(Ok(Ok(()))) => Ok(output),
            // git stopping early (a 128 on a bad signature) cuts the write
            // short, and its own status says so.
            Some(Ok(Err(_))) if !output.status.success() => Ok(output),
            Some(Ok(Err(e))) => Err(format!(
                "{} succeeded without reading all its input: {e}",
                label()
            )),
            Some(Err(_)) => Err(format!("{} input writer panicked", label())),
        }
    }
}

/// Whether `handle`'s thread finishes by `deadline`.
fn joins_by<T>(handle: &std::thread::JoinHandle<T>, deadline: std::time::Instant) -> bool {
    while !handle.is_finished() {
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(GUARD_GIT_POLL);
    }
    true
}

/// Why the issue branch's commits break the repository's signing policy:
/// `Ok(None)` when they keep it or there is no policy, `Ok(Some(violation))`
/// for the model to fix, and `Err` when the guard could not judge them.
///
/// Measured 2026-10-26: Muse's sandboxed shell could not sign (`No user
/// exists for uid 501`), and the model silently retried with
/// `-c commit.gpgsign=false`. Any implementer can do that, so this is checked
/// for every harness. `%G?` is not a reliable signed/unsigned test (it reads
/// `U` or `N` for a valid SSH signature without `gpg.ssh.allowedSignersFile`),
/// so presence comes from the commit's `gpgsig` (or, in a SHA-256 repository,
/// `gpgsig-sha256`) header and `%G?` only flags `B`, a bad signature.
///
/// The commits checked are the ones that ship: everything reachable from the
/// issue branch (`refs/heads/<branch>`, never `HEAD`, which the IC may have
/// left elsewhere) and from its own remote-tracking refs
/// (`refs/remotes/<remote>/<branch>` for each configured remote: what was
/// pushed and what the PR is opened from), but not from the cut
/// ([`Worktree::cut`], recorded once when the branch was made) nor from any
/// other remote-tracking ref. So upstream commits the agent merges or rebases
/// in are never blamed on it, an earlier dispatch's bypass stays caught after
/// it is pushed (or after the local branch is reset), and a moving operator
/// checkout cannot shrink or widen the range. A missing issue branch is a
/// violation: nothing that ships can be checked.
///
/// A flagged commit is the model's only when it was committed as this run's
/// identity (`git var GIT_COMMITTER_IDENT`, or the controller's
/// `GIT_COMMITTER_EMAIL`). One a collaborator pushed is still never accepted,
/// but it is [`GuardFault::ForeignUnsigned`], not the model's failed attempt:
/// fixing it means rewriting their commit. An identity that cannot be read
/// counts every commit as the model's. This assumes an implementer commits
/// as the operator: measured 2026-10-27 on two real Muse commits, whose
/// committer was exactly the worktree's `git var GIT_COMMITTER_IDENT`.
/// Codex is unmeasured; it runs with the same inherited configuration and
/// the same scrubbed `GIT_*` environment, so it is assumed to match. A
/// mismatch never lets a `met` through: it only stops the run instead of
/// charging an attempt, and the stop names both identities.
///
/// In a mixed range, where some flagged commits are the model's and some a
/// collaborator's, the violation names them all, the collaborator's
/// included, and the `met` is the model's failed attempt: fail closed, since
/// every one of them would ship unsigned.
///
/// The policy is enforced when it was on at this run's start (`seen`), when
/// the branch or the repository recorded it on ([`worktree::record_signing`],
/// at creation and at each run's start), or when it is on now; an enforced
/// policy that is now off is a violation of its own, across runs.
///
/// The flags survive a deliberate operator change to `commit.gpgsign` too. To
/// stop enforcing on a branch, run `git config branch.<branch>.ironmemSigning
/// false` (with the policy off).
///
/// `Err` is never the model's fault: the caller makes it an infrastructure
/// failure, which never counts as `met` and consumes no attempt, and stops
/// the run on one that every later `met` would meet too
/// ([`GuardFault::stops_run`]).
///
/// This catches a lazy or accidental bypass, not deliberate tampering. An
/// unconfined agent could just as well opt its branch out, rewrite
/// `branch.<branch>.ironmemCut` to hide earlier commits, push its commits to
/// another remote branch (every remote-tracking ref but the issue branch's
/// own is excluded via `--remotes`), commit under another identity, or sign
/// with a throwaway key.
fn signing_violation(
    worktree: &Worktree,
    seen: SigningPolicySeen,
) -> Result<Option<SigningViolation>, GuardFault> {
    check_signing(worktree, seen, SIGNING_GUARD_TIMEOUT)
}

/// [`signing_violation`] with each git command bounded by `timeout`.
fn check_signing(
    worktree: &Worktree,
    seen: SigningPolicySeen,
    timeout: std::time::Duration,
) -> Result<Option<SigningViolation>, GuardFault> {
    match worktree::signing_policy(&worktree.path) {
        Err(why) => {
            return Err(GuardFault::PolicyUnreadable(format!(
                "commit.gpgsign could not be read: {why}"
            )))
        }
        Ok(false) => {
            let holder = match worktree::signing_holder(&worktree.path, &worktree.branch)
                .map_err(GuardFault::PolicyUnreadable)?
            {
                worktree::SigningHold::Held(holder) => Some(holder),
                // An operator's opt-out releases what an interrupted run
                // saw, but not a run that is still going and saw it on.
                worktree::SigningHold::OptedOut => seen.fresh().holder().map(str::to_string),
                worktree::SigningHold::NotHeld => seen.holder().map(str::to_string),
            };
            return Ok(holder.map(|holder| SigningViolation::PolicyDisabled { holder }));
        }
        Ok(true) => {}
    }
    let unreadable = GuardFault::GitUnreadable;
    let git = GuardGit::new(&worktree.path, timeout).map_err(unreadable)?;
    let branch = worktree.branch.as_str();
    let local = format!("refs/heads/{branch}");
    let probe = git
        .run(
            &[
                "rev-parse",
                "--verify",
                "--quiet",
                &format!("{local}^{{commit}}"),
            ],
            None,
        )
        .map_err(unreadable)?;
    match probe.status.code() {
        Some(0) => {}
        Some(1) => {
            return Ok(Some(SigningViolation::MissingBranch {
                branch: branch.to_string(),
            }))
        }
        _ => return Err(unreadable(worktree::bounded_stderr(&probe.stderr))),
    }
    let read = |args: &[&str]| -> Result<String, GuardFault> {
        let out = git.run(args, None).map_err(unreadable)?;
        if !out.status.success() {
            return Err(unreadable(worktree::bounded_stderr(&out.stderr)));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    // The issue branch's own remote-tracking refs: exactly
    // `refs/remotes/<remote>/<branch>` for a configured remote, so a ref
    // such as `origin/x/<branch>` (another branch) is never taken for it.
    let configured = read(&["remote"])?;
    let configured: Vec<&str> = configured.lines().map(str::trim).collect();
    let refs = read(&["for-each-ref", "--format=%(refname)", "refs/remotes/"])?;
    let refs: std::collections::HashSet<&str> = refs.lines().collect();
    let own: Vec<(&str, String)> = configured
        .iter()
        .filter_map(|remote| {
            let refname = format!("refs/remotes/{remote}/{branch}");
            refs.contains(refname.as_str())
                .then_some((*remote, refname))
        })
        .collect();
    // `--no-show-signature` on every `git log`: an operator's
    // `log.showSignature=true` prints verification lines ahead of each
    // signed commit, even under `--format`. Listing without `%G?` also keeps
    // a commit whose signature git cannot parse from failing the whole range
    // (`%G?` exits 128 on it); that commit is judged on its own below.
    let mut args = vec![
        "log".to_string(),
        "--no-show-signature".into(),
        "--topo-order".into(),
        "--format=%H %ce".into(),
        local.clone(),
    ];
    args.extend(own.iter().map(|(_, refname)| refname.clone()));
    // Every ref ending in the branch is kept out of the negated set, not only
    // the configured remotes' own: a lookalike can only hide commits there.
    args.extend([
        "--not".into(),
        worktree.cut.clone(),
        format!("--exclude=*/{branch}"),
        "--remotes".into(),
    ]);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    // Children before parents, so the last one flagged has no flagged ancestor.
    let listed = read(&args)?;
    let commits: Vec<(&str, &str)> = listed
        .lines()
        .filter_map(|line| {
            let (sha, committer) = line.trim().split_once(' ').unwrap_or((line.trim(), ""));
            is_full_object_name(sha).then_some((sha, committer))
        })
        .collect();
    let shas: Vec<&str> = commits.iter().map(|(sha, _)| *sha).collect();
    // Every commit object in one read, so a long range costs two git
    // processes rather than two per commit.
    let headers = git
        .run(&["cat-file", "--batch"], Some(&shas))
        .map_err(unreadable)?;
    if !headers.status.success() {
        return Err(unreadable(worktree::bounded_stderr(&headers.stderr)));
    }
    let has_header = signature_headers(&headers.stdout, &shas).map_err(unreadable)?;
    let with_header: Vec<&str> = shas
        .iter()
        .zip(&has_header)
        .filter_map(|(sha, has)| has.then_some(*sha))
        .collect();
    // `%G?` for every commit that carries a header, in one read. A signature
    // git cannot even parse fails that whole read with 128, so those ranges
    // are read one commit at a time, where the failure names its commit.
    let mut statuses = std::collections::HashMap::new();
    if !with_header.is_empty() {
        let batch = git
            .run(
                &[
                    "log",
                    "--no-walk",
                    "--stdin",
                    "--no-show-signature",
                    "--format=%H %G?",
                ],
                Some(&with_header),
            )
            .map_err(unreadable)?;
        match batch.status.code() {
            Some(0) => statuses = signature_statuses(&batch.stdout),
            Some(128) => {}
            _ => return Err(unreadable(worktree::bounded_stderr(&batch.stderr))),
        }
    }
    let mut flagged = Vec::new();
    for ((sha, committer), has) in commits.iter().zip(has_header) {
        let signed = has
            && match statuses.get(*sha) {
                Some(status) => status != "B",
                None => {
                    let one = git
                        .run(
                            &["log", "-1", "--no-show-signature", "--format=%G?", sha],
                            None,
                        )
                        .map_err(unreadable)?;
                    signature_kept(&one).map_err(unreadable)?
                }
            };
        if !signed {
            flagged.push((prefix(sha, SHORT_SHA_CHARS), *committer));
        }
    }
    if flagged.is_empty() {
        return Ok(None);
    }
    let identity = run_identity(&git);
    let own_commit = |committer: &str| {
        identity
            .as_ref()
            .is_none_or(|emails| emails.iter().any(|e| e.eq_ignore_ascii_case(committer)))
    };
    if !flagged.iter().any(|(_, committer)| own_commit(committer)) {
        return Err(GuardFault::ForeignUnsigned {
            commits: flagged
                .into_iter()
                .map(|(sha, committer)| (sha, committer.to_string()))
                .collect(),
            identity: identity.unwrap_or_default().join(" or "),
        });
    }
    // The recipe pushes to the remote that already tracks the issue branch
    // (`origin` first when several do), else the only remote, else `origin`.
    let remote = own
        .iter()
        .map(|(remote, _)| *remote)
        .find(|remote| *remote == "origin")
        .or_else(|| own.first().map(|(remote, _)| *remote))
        .or_else(|| (configured.len() == 1).then(|| configured[0]))
        .unwrap_or("origin");
    Ok(Some(SigningViolation::Unsigned {
        flagged: flagged.into_iter().map(|(sha, _)| sha).collect(),
        cut: worktree.cut.clone(),
        branch: branch.to_string(),
        remote: remote.to_string(),
    }))
}

/// Run-start collaborator checks already found clean, by worktree and
/// branch: the [`range_key`] each was judged on. In this process only, so a
/// Lead re-checks an unchanged branch once, not on every tick.
static CLEAN_AT_START: std::sync::Mutex<
    std::collections::BTreeMap<(std::path::PathBuf, String), String>,
> = std::sync::Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
thread_local! {
    /// How many run-start checks read the range in full, on this thread.
    static START_CHECKS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A collaborator's unsigned commit on the branch at a run's start, which
/// would turn away any `met` the run produced. The range is read in full only
/// when something it depends on changed since it was last found clean: every
/// ref tip, the remotes, the cut, the policy, the identity and the pinned
/// signature programs ([`range_key`]). Fail closed: a key that cannot be read
/// is never cached, and a fault is never remembered as clean.
fn collaborator_check_at_start(worktree: &Worktree, seen: SigningPolicySeen) -> Option<GuardFault> {
    let slot = (worktree.path.clone(), worktree.branch.clone());
    let key = range_key(worktree).ok();
    let cache = || CLEAN_AT_START.lock().unwrap_or_else(|e| e.into_inner());
    if key.is_some() && cache().get(&slot) == key.as_ref() {
        return None;
    }
    #[cfg(test)]
    START_CHECKS.with(|n| n.set(n.get() + 1));
    match signing_violation(worktree, seen) {
        Err(fault @ GuardFault::ForeignUnsigned { .. }) => {
            cache().remove(&slot);
            Some(fault)
        }
        Err(_) => None,
        Ok(_) => {
            if let Some(key) = key {
                cache().insert(slot, key);
            }
            None
        }
    }
}

/// Everything the collaborator check's answer depends on, as one string.
fn range_key(worktree: &Worktree) -> Result<String, String> {
    let git = GuardGit::new(&worktree.path, SIGNING_GUARD_TIMEOUT)?;
    let read = |args: &[&str]| -> Result<String, String> {
        let out = git.run(args, None)?;
        if !out.status.success() {
            return Err(worktree::bounded_stderr(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    let refs = read(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        &format!("refs/heads/{}", worktree.branch),
        "refs/remotes/",
    ])?;
    let remotes = read(&["remote"])?;
    // The ident without its timestamp, which changes every second.
    let ident = read(&["var", "GIT_COMMITTER_IDENT"])?;
    let ident = ident.split_once('>').map_or("", |(who, _)| who);
    let policy = worktree::signing_policy(&worktree.path)?;
    Ok(format!(
        "{refs}\0{remotes}\0{ident}\0{:?}\0{}\0{policy}\0{:?}",
        std::env::var("GIT_COMMITTER_EMAIL").ok(),
        worktree.cut,
        git.pins
    ))
}

/// The committer emails this run's commits carry: the worktree's
/// `git var GIT_COMMITTER_IDENT`, plus the controller's own
/// `GIT_COMMITTER_EMAIL`, which an implementer that inherits the
/// controller's environment would commit under. `None` when neither reads.
fn run_identity(git: &GuardGit<'_>) -> Option<Vec<String>> {
    let mut emails = Vec::new();
    if let Ok(out) = git.run(&["var", "GIT_COMMITTER_IDENT"], None) {
        let ident = String::from_utf8_lossy(&out.stdout);
        if let Some((_, rest)) = ident.split_once('<') {
            if let Some((email, _)) = rest.split_once('>') {
                if out.status.success() && !email.trim().is_empty() {
                    emails.push(email.trim().to_string());
                }
            }
        }
    }
    if let Ok(email) = std::env::var("GIT_COMMITTER_EMAIL") {
        if !email.trim().is_empty() {
            emails.push(email.trim().to_string());
        }
    }
    (!emails.is_empty()).then_some(emails)
}

/// Whether one commit's own `git log -1 --format=%G?` read keeps its
/// signature: `true` unless it reads `B`, or git refused it as a signature it
/// cannot parse (the commit's fault, flagged like a bad one). Any other
/// failure is the guard's, not the commit's.
fn signature_kept(out: &std::process::Output) -> Result<bool, String> {
    match out.status.code() {
        Some(0) => Ok(String::from_utf8_lossy(&out.stdout).trim() != "B"),
        Some(128)
            if String::from_utf8_lossy(&out.stderr).contains("bad/incompatible signature") =>
        {
            Ok(false)
        }
        _ => Err(worktree::bounded_stderr(&out.stderr)),
    }
}

/// From `git cat-file --batch` output for `shas` (in that order), whether
/// each commit carries a signature header. Anything but one commit object
/// per requested name, in order, is unreadable rather than guessed at.
fn signature_headers(mut out: &[u8], shas: &[&str]) -> Result<Vec<bool>, String> {
    let mut found = Vec::with_capacity(shas.len());
    for sha in shas {
        let newline = out
            .iter()
            .position(|&b| b == b'\n')
            .ok_or_else(|| format!("cat-file ended before {sha}"))?;
        let header = String::from_utf8_lossy(&out[..newline]).into_owned();
        let mut words = header.split(' ');
        let (name, kind, size) = (words.next(), words.next(), words.next());
        if name != Some(sha) || kind != Some("commit") {
            return Err(format!(
                "cat-file answered {:?} for commit {sha}",
                prefix(&header, 120)
            ));
        }
        let size: usize = size
            .and_then(|size| size.parse().ok())
            .ok_or_else(|| format!("cat-file gave no size for {sha}"))?;
        let body = out
            .get(newline + 1..newline + 1 + size)
            .ok_or_else(|| format!("cat-file output for {sha} is truncated"))?;
        found.push(has_signature_header(&String::from_utf8_lossy(body)));
        out = out.get(newline + 2 + size..).unwrap_or_default();
    }
    Ok(found)
}

/// `%G?` per commit from `git log --format='%H %G?'`. Only lines that are an
/// object name and one status letter count, so stray verification output can
/// never be read as a status; a commit missing here is read on its own.
fn signature_statuses(out: &[u8]) -> std::collections::HashMap<String, String> {
    String::from_utf8_lossy(out)
        .lines()
        .filter_map(|line| {
            let (sha, status) = line.trim().split_once(' ')?;
            (is_full_object_name(sha) && status.len() == 1)
                .then(|| (sha.to_string(), status.to_string()))
        })
        .collect()
}

/// Whether `text` is a full object name: 40 hex digits, or 64 in a SHA-256
/// repository. Anything else in `git log` output is not a commit to check.
fn is_full_object_name(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Whether a raw commit object carries a signature header: `gpgsig`, or
/// `gpgsig-sha256` in a SHA-256 repository. Only the header block counts.
fn has_signature_header(commit_object: &str) -> bool {
    commit_object
        .lines()
        .take_while(|l| !l.is_empty())
        .any(|l| l.starts_with("gpgsig ") || l.starts_with("gpgsig-sha256 "))
}

/// How many flagged commits a violation names before counting the rest.
const NAMED_UNSIGNED_COMMITS: usize = 5;

/// The violation for `flagged` short shas, newest first. The recipe re-signs
/// only those commits, on the issue `branch`, and pushes it to `remote` by
/// name: rebasing from the cut would rewrite the upstream commits merged in,
/// which the guard deliberately does not blame. It is a plain rebase, not
/// `--rebase-merges`: that would also re-pick, and so rewrite, the upstream
/// commits on a merged-in side, so a merge is redone by hand instead.
fn unsigned_violation(flagged: &[String], cut: &str, branch: &str, remote: &str) -> String {
    let mut list = flagged
        .iter()
        .take(NAMED_UNSIGNED_COMMITS)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if flagged.len() > NAMED_UNSIGNED_COMMITS {
        list.push_str(&format!(
            " and {} more",
            flagged.len() - NAMED_UNSIGNED_COMMITS
        ));
    }
    let oldest = flagged.last().map(String::as_str).unwrap_or_default();
    let short_cut = prefix(cut, SHORT_SHA_CHARS);
    format!(
        "commit.gpgsign is on, but commit(s) {list} on this branch since its cut {short_cut} are \
         unsigned or carry a bad signature. Re-sign only these commits, never upstream ones \
         merged in. First `git checkout {branch}`. Then, if only the tip is flagged, `git commit \
         --amend --no-edit -S`; otherwise `git rebase --exec 'git commit --amend --no-edit -S' \
         {oldest}^`. If a merge commit is flagged or lies in that rebase's range, redo the merge \
         (signed) instead of rebasing over it. Then `git push --force-with-lease {remote} \
         {branch}`. If signing fails in your environment, return \
         verdict impossible with the signing error; never bypass with -c commit.gpgsign=false, \
         --no-gpg-sign, or by changing git config."
    )
}

/// The prior-attempt context the turn prompt shows the IC, newest-last, so a
/// resumed dispatch reads what has already been tried before doing anything.
fn prior_attempts(db: &Database, issue: &IssueRef) -> Result<Vec<PriorAttempt>, MemoryError> {
    Ok(lineage::attempts_for_issue(db, issue)?
        .into_iter()
        .map(|record| PriorAttempt {
            attempt_n: record.attempt_n,
            approach: record.approach,
            verdict: record.verdict,
            why_failed: record.why_failed,
        })
        .collect())
}

/// The IC's own account of what it did, for the lineage record's `approach`.
/// The schema's `reason` string is the only first-hand description a dispatch
/// returns; when it is absent (no verdict was produced at all) this
/// synthesizes the little that is known rather than persisting an empty
/// field.
fn approach_text(outcome: &DispatchOutcome, classification: DispatchClassification) -> String {
    match outcome.reason.as_deref() {
        Some(reason) if !reason.trim().is_empty() => reason.to_string(),
        _ => format!(
            "dispatch returned no verdict reason ({classification:?}); {} turns, session {}",
            outcome.num_turns,
            outcome.native_session_id.as_deref().unwrap_or("unknown")
        ),
    }
}

/// Why an attempt failed, for the lineage record's `why_failed`. Names the
/// classification explicitly so a later reader (or the next dispatch's own
/// prompt) can tell "the evaluator judged this impossible" apart from "the
/// gate simply did not pass yet" without re-deriving it from the prose.
fn why_failed_text(outcome: &DispatchOutcome, classification: DispatchClassification) -> String {
    let headline = match classification {
        DispatchClassification::Impossible => "goal evaluator judged the gate condition impossible",
        DispatchClassification::FailedAttempt if outcome.verdict.is_none() => {
            "dispatch ended with no verdict (goal still set)"
        }
        _ => "gate condition not met",
    };
    match outcome.reason.as_deref() {
        Some(reason) if !reason.trim().is_empty() => format!("{headline}: {reason}"),
        _ => headline.to_string(),
    }
}

/// The lineage `approach` and `why_failed` for a `met` the signing guard
/// refused. The violation goes in `why_failed` only, and the model's own
/// reason, bounded by [`MODEL_REASON_CHARS`], is the approach: written into
/// both, a long violation made the prior-attempt line too long for the next
/// turn's prompt, which then dropped it and never said why the `met` failed.
fn refused_met_record(violation: &str, original: Option<String>) -> (String, String) {
    let approach = match original {
        Some(original) if !original.trim().is_empty() => prefix(&original, MODEL_REASON_CHARS),
        _ => "reported the gate met, giving no reason".to_string(),
    };
    (
        approach,
        format!("the commit-signing guard refused the met: {violation}"),
    )
}

/// Persist one attempt: the append-only lineage record plus the issue's
/// current-state drawer, whose `cumulative_attempt_n` is the spec's
/// cross-dispatch stagnation counter. `prior` is the issue's status as read
/// *before* this attempt, which is what makes the no-downgrade rule below
/// possible.
///
/// `best_verdict`/`best_commit_sha` are never downgraded: once an issue has
/// recorded a success, a later failed attempt (a re-run, a follow-up fix
/// dispatch) must not erase it — the field's whole purpose is "best so far",
/// not "most recent".
fn record_and_advance(
    db: &Database,
    record: &AttemptRecord,
    prior: Option<&IssueStatus>,
    recovering_harness: Option<Implementer>,
) -> Result<(), MemoryError> {
    // A recovering harness (Codex, Muse) can replay durable artifacts after a crash between the append-only
    // attempt and current-status writes. Complete that same ordinal instead
    // of appending it twice. Terminal summaries are markers, not attempts.
    let existing = if recovering_harness.is_some() {
        recorded_attempt(db, &record.issue, record.attempt_n)?
    } else {
        None
    };
    if existing
        .as_ref()
        .is_some_and(|attempt| attempt.verdict != record.verdict)
    {
        let Some(harness) = recovering_harness else {
            unreachable!("only a recovering harness looks up an existing ordinal");
        };
        debug_assert!(harness.recovers_invocations());
        let label = harness.label();
        return Err(MemoryError::Validation(format!(
            "recovered {label} verdict conflicts with the already recorded attempt ordinal"
        )));
    }
    if existing.is_none() {
        lineage::record_attempt(db, record)?;
    }
    let record = existing.as_ref().unwrap_or(record);

    let issue = &record.issue;
    let verdict = record.verdict;
    let prior_success =
        prior.and_then(|status| status.best_verdict) == Some(AttemptOutcome::Success);
    let (best_verdict, best_commit_sha) = if verdict == AttemptOutcome::Success {
        (Some(AttemptOutcome::Success), record.commit_sha.clone())
    } else if prior_success {
        (
            Some(AttemptOutcome::Success),
            prior.and_then(|status| status.best_commit_sha.clone()),
        )
    } else {
        (
            Some(AttemptOutcome::Failed),
            prior.and_then(|status| status.best_commit_sha.clone()),
        )
    };

    lineage::upsert_issue_status(
        db,
        &IssueStatus {
            issue: issue.clone(),
            best_verdict,
            best_commit_sha,
            cumulative_attempt_n: record.attempt_n,
        },
    )?;
    Ok(())
}

/// The attempt already recorded at ordinal `attempt_n`, if any. Attempt-cap
/// terminal summaries are markers, not attempts, so they never match.
fn recorded_attempt(
    db: &Database,
    issue: &IssueRef,
    attempt_n: u32,
) -> Result<Option<AttemptRecord>, MemoryError> {
    Ok(lineage::attempts_for_issue(db, issue)?
        .into_iter()
        .find(|attempt| attempt.attempt_n == attempt_n && !is_terminal_summary(&attempt.approach)))
}

/// Opening words of the attempt-cap terminal record's `approach`. A const
/// because it is written by [`record_terminal_summary`] and read back by
/// [`is_terminal_summary`] to keep the record from being appended twice.
const TERMINAL_SUMMARY_PREFIX: &str = "terminal: per-issue attempt cap";

/// Whether a recorded attempt's `approach` marks it as an attempt-cap
/// terminal record rather than a real try.
///
/// Takes the `approach` string rather than a record type because both
/// [`PriorAttempt`] and [`AttemptRecord`] readers need the same answer —
/// [`super::supervise::assess_strategy_health`] must exclude these markers
/// from its thrash window, since a marker's `why_failed` quotes every attempt
/// before it and would otherwise look like a repetition of them.
///
/// `pub` rather than `pub(super)` because `autopilot advise` renders the same
/// lineage into the same prompt the Lead does, and a preview that quoted
/// these markers as approaches would disagree with the tick it is previewing.
pub fn is_terminal_summary(approach: &str) -> bool {
    approach.starts_with(TERMINAL_SUMMARY_PREFIX)
}

/// The spec's *Cross-dispatch stagnation control* terminal record: on
/// reaching the cap, "append a terminal lineage record, post a comment
/// summarizing everything tried". This writes the lineage half; the GitHub
/// comment and the `agent:exhausted` label are rung 6's.
///
/// It shares `attempt_n` with the final attempt rather than claiming a new
/// one — it is a marker summarizing the run, not another try, and
/// incrementing the cumulative counter past the cap would make the stored
/// state contradict the cap it just reported hitting. `attempt_cap` is
/// carried separately only for the message, since `attempt_n` is the lifetime
/// number and sits above the cap after a retry, a remediation, or a cap
/// lowered between runs.
pub(super) fn record_terminal_summary(
    db: &Database,
    issue: &IssueRef,
    attempt_n: u32,
    attempt_cap: u32,
    attempts: &[PriorAttempt],
) -> Result<(), MemoryError> {
    // Written once, not once per invocation. An exhausted issue is a stable
    // state that a polling Lead re-reads every pass, and each terminal
    // record's summary quotes every attempt before it — including earlier
    // terminal records — so appending a second one would nest the first
    // one's whole text inside it and grow the lineage (and the prior-attempt
    // prompt every later dispatch reads) on every single re-run.
    // Bounded by attempt number rather than by mere presence. A human retry
    // (`autopilot retry`) lets an issue exhaust its cap a *second* time, with
    // real attempts recorded after the first summary; suppressing on presence
    // alone would leave that second exhaustion with no record at all, and the
    // stale summary — which quotes only the pre-retry attempts — as the
    // lineage's last word. Re-running an already-exhausted issue still
    // appends nothing, because its summary sits at the same attempt number.
    if attempts
        .iter()
        .any(|a| is_terminal_summary(&a.approach) && a.attempt_n >= attempt_n)
    {
        return Ok(());
    }
    let summary = if attempts.is_empty() {
        "no attempts recorded".to_string()
    } else {
        attempts
            .iter()
            .map(|attempt| {
                let verdict = match attempt.verdict {
                    AttemptOutcome::Success => "success",
                    AttemptOutcome::Failed => "failed",
                };
                match attempt.why_failed.as_deref() {
                    Some(why) => format!("#{} {verdict}: {why}", attempt.attempt_n),
                    None => format!("#{} {verdict}: {}", attempt.attempt_n, attempt.approach),
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    lineage::record_attempt(
        db,
        &AttemptRecord {
            issue: issue.clone(),
            attempt_n,
            approach: format!(
                "{TERMINAL_SUMMARY_PREFIX} ({attempt_cap}) reached; this record summarizes \
                 every attempt and is a marker, not a further attempt"
            ),
            verdict: AttemptOutcome::Failed,
            why_failed: Some(summary),
            commit_sha: None,
        },
    )?;
    Ok(())
}

/// The repo's approved gate commands, or an error naming the exact next
/// command a human should run.
///
/// This is the spec's *"The Lead refuses to dispatch into any repo without
/// an `approved` config"* and its Testing table's *"Work refused on a
/// `pending` (unapproved) repo config"*, and it is the safety-critical
/// check in this module: a wrong or absent gate means confidently committing
/// broken code.
///
/// It is public, and separate from [`run_issue`], so that a caller which
/// does work *before* dispatching — provisioning a git worktree, most
/// obviously — can refuse at the same point [`run_issue`] would rather than
/// leaving a checkout and a branch behind for a repo that was never
/// eligible. [`run_issue`] calls it again itself; the check is cheap and
/// duplicating it is much better than depending on every caller to have
/// remembered.
pub fn approved_gate_commands(db: &Database, repo: &str) -> Result<Vec<String>, MemoryError> {
    let gate = gate_config::get_gate_config(db, repo)?.ok_or_else(|| {
        MemoryError::Validation(format!(
            "no gate config for '{repo}' — run `ironmem autopilot onboard {repo}` first"
        ))
    })?;
    if gate.state != gate_config::GateConfigState::Approved {
        return Err(MemoryError::Permission(format!(
            "gate config for '{repo}' is {:?}, not approved — run `ironmem autopilot approve \
             {repo}` before dispatching",
            gate.state
        )));
    }
    Ok(gate.gate_commands().to_vec())
}

/// Validate ownership of any persisted dispatch before a caller provisions
/// or replaces the issue worktree. Returning the state lets callers perform
/// harness-specific liveness checks without a second database read.
pub fn validate_dispatch_owner(
    db: &Database,
    issue: &IssueRef,
    implementer: Implementer,
) -> Result<Option<DispatchState>, MemoryError> {
    let state = dispatch_state::get_dispatch_state(db, issue)?;
    if let Some(state) = &state {
        if state.implementer != implementer {
            return Err(MemoryError::Validation(format!(
                "implementation harness mismatch: persisted {:?}, configured {:?}; resume with \
                 the persisted harness or clear its state with `ironmem autopilot \
                 clear-dispatch {} {}`",
                state.implementer, implementer, issue.repo, issue.number
            )));
        }
    }
    Ok(state)
}

/// Refuse unsafe worktree provisioning for a persisted supervised CLI dispatch.
///
/// A dirty worktree may belong to a still-running process. Quarantining it
/// before checking the persisted invocation would move that process's cwd and
/// detach its branch. Completed or explicitly idle pauses may proceed; live or
/// uncertain evidence holds without touching the checkout.
pub fn preflight_worktree_provisioning(
    db: &Database,
    issue: &IssueRef,
    implementer: Implementer,
    dispatcher: &mut dyn Dispatcher,
) -> Result<(), MemoryError> {
    if dispatcher.implementer() != implementer {
        return Err(MemoryError::Validation(format!(
            "implementation harness mismatch: configured {:?}, dispatcher {:?}; select the \
             matching dispatcher",
            implementer,
            dispatcher.implementer()
        )));
    }
    let Some(state) = validate_dispatch_owner(db, issue, implementer)? else {
        return Ok(());
    };
    if !implementer.recovers_invocations() {
        return Ok(());
    }
    match dispatcher.recover(Path::new(&state.worktree_path), &state)? {
        InvocationRecovery::Alive => Err(MemoryError::Validation(format!(
            "cannot provision {} while its {} implementation is still alive",
            issue.canonical(),
            implementer.label()
        ))),
        InvocationRecovery::Unknown { reason } => Err(MemoryError::Validation(format!(
            "cannot provision {} while {} process completion is uncertain: {reason}; {}",
            issue.canonical(),
            implementer.label(),
            release_hint(issue, state.invocation_artifacts.as_ref())
        ))),
        InvocationRecovery::Finished(_) => Ok(()),
        InvocationRecovery::NoPriorInvocation if state.invocation_artifacts.is_none() => Ok(()),
        InvocationRecovery::NoPriorInvocation => Err(MemoryError::Validation(format!(
            "{} recovery returned no invocation for persisted artifacts; process completion \
             is unproven",
            implementer.label()
        ))),
    }
}

/// How a held supervised invocation is released. One held only because its
/// worktree was not quiet finishes by itself; any other needs the operator
/// to confirm nothing is running first.
fn release_hint(issue: &IssueRef, artifacts: Option<&InvocationArtifacts>) -> String {
    if artifacts.is_some_and(super::invocation::is_awaiting_quiet) {
        format!(
            "a later Lead tick or `ironmem autopilot run` finishes it once nothing it started \
             remains in the worktree; `ironmem autopilot clear-dispatch {} {}` is the manual \
             release",
            issue.repo, issue.number
        )
    } else {
        format!(
            "once no process remains, `ironmem autopilot clear-dispatch {} {}` releases it",
            issue.repo, issue.number
        )
    }
}

/// What a run refused at its start (before recovery) leaves behind, said
/// for the harness and the dispatch state it found: only a supervised
/// harness's pending invocation is recovered.
fn kept_at_start(implementer: Implementer, state: Option<&DispatchState>) -> &'static str {
    match state {
        Some(state)
            if implementer.recovers_invocations() && state.invocation_artifacts.is_some() =>
        {
            "a pending invocation for this issue is kept and will be recovered once this is \
             fixed; do not clear-dispatch it"
        }
        Some(_) => {
            "the issue's dispatch state is left as it is, and the next run after the fix \
             continues from it"
        }
        None => "nothing was written, so the next run after the fix starts the issue afresh",
    }
}

/// What a run stopped after a dispatch by a guard fault leaves behind.
fn kept_mid_run(implementer: Implementer) -> &'static str {
    if implementer.recovers_invocations() {
        "the finished invocation is kept, and the first run after the fix checks its met \
         again; do not clear-dispatch it"
    } else {
        "the session is kept (state `infrastructure-failure`), and the next run after the fix \
         resumes it"
    }
}

/// What restart recovery decided before a run dispatches anything.
enum Recovered {
    /// The run ends here without dispatching.
    Done(IssueRun),
    /// The run continues, first classifying a finished invocation if any.
    Continue(Option<DispatchOutcome>),
}

/// Adopt, hold, or finish the invocation a persisted state names, for a
/// harness that [recovers invocations](Implementer::recovers_invocations).
#[allow(clippy::too_many_arguments)]
fn recover_invocation(
    db: &Database,
    issue: &IssueRef,
    worktree: &Worktree,
    dispatcher: &mut dyn Dispatcher,
    state: &DispatchState,
    cumulative_attempt_n: u32,
    wall_clock_timeout_secs: Option<u64>,
    native_session_id: &mut Option<String>,
    invocation_artifacts: &mut Option<InvocationArtifacts>,
) -> Result<Recovered, MemoryError> {
    match dispatcher.recover(&worktree.path, state)? {
        recovery @ (InvocationRecovery::Alive | InvocationRecovery::Unknown { .. }) => {
            let detail = match recovery {
                InvocationRecovery::Unknown { reason } => format!(
                    "{reason}; {}",
                    release_hint(issue, state.invocation_artifacts.as_ref())
                ),
                _ => format!(
                    "{} implementation is still alive",
                    state.implementer.label()
                ),
            };
            let unpriced_dispatches = match &state.invocation_artifacts {
                Some(artifacts) => u32::from(budget::has_unpriced_dispatch_reservation(
                    db,
                    &artifacts.invocation_id,
                )?),
                None => 0,
            };
            Ok(Recovered::Done(IssueRun {
                unpriced_dispatches,
                issue: issue.clone(),
                terminal: TerminalReason::ImplementationInFlight { detail },
                dispatches: Vec::new(),
                total_cost_usd: 0.0,
                cumulative_attempt_n,
                wall_clock_timeout_secs,
            }))
        }
        InvocationRecovery::Finished(outcome) => {
            if state.attempt_n < cumulative_attempt_n {
                // The lineage/status write completed before the Lead crashed
                // clearing this artifact. Its terminal Impossible decision
                // still ends this run, without recording or charging again.
                // Apply the completion and identity checks before trusting
                // the verdict; a later manual run is allowed after clearing.
                if outcome.invocation_started
                    && outcome.process_success
                    && !outcome.is_error
                    && !outcome.timed_out
                    && outcome.is_impossible()
                    && outcome.native_session_id.as_ref().is_some_and(|id| {
                        !id.trim().is_empty()
                            && native_session_id
                                .as_ref()
                                .is_none_or(|expected| expected == id)
                    })
                {
                    // Only the state recovery judged; one another run wrote
                    // since is theirs (every write is a compare-and-set).
                    dispatch_state::clear_dispatch_state_if_unchanged(db, state)?;
                    return Ok(Recovered::Done(IssueRun {
                        unpriced_dispatches: 1,
                        issue: issue.clone(),
                        terminal: TerminalReason::Impossible {
                            reason: outcome.reason,
                        },
                        dispatches: Vec::new(),
                        total_cost_usd: 0.0,
                        cumulative_attempt_n,
                        wall_clock_timeout_secs,
                    }));
                }
                // Preserve the proven conversation for a continuing run,
                // but never classify or charge the same attempt again.
                if native_session_id.is_none() {
                    *native_session_id =
                        outcome.native_session_id.filter(|id| !id.trim().is_empty());
                }
                *invocation_artifacts = None;
                Ok(Recovered::Continue(None))
            } else {
                Ok(Recovered::Continue(Some(outcome)))
            }
        }
        InvocationRecovery::NoPriorInvocation if state.invocation_artifacts.is_none() => {
            Ok(Recovered::Continue(None))
        }
        InvocationRecovery::NoPriorInvocation => Err(MemoryError::Validation(format!(
            "{} recovery returned no invocation for persisted artifacts; process \
             completion is unproven",
            state.implementer.label()
        ))),
    }
}

/// Drive one issue to a terminal state.
///
/// Refuses outright, before writing anything, if the repo has no approved
/// gate config — see [`approved_gate_commands`].
pub fn run_issue(
    db: &Database,
    issue: &IssueRef,
    brief: &IssueBrief,
    worktree: &Worktree,
    config: &RunConfig,
    dispatcher: &mut dyn Dispatcher,
) -> Result<IssueRun, MemoryError> {
    super::validate_repo(&issue.repo)?;
    config.validate()?;

    if config.implementer != dispatcher.implementer() {
        return Err(MemoryError::Validation(format!("implementation harness mismatch: configured {:?}, dispatcher {:?}; select the matching dispatcher", config.implementer, dispatcher.implementer())));
    }
    let existing_state = validate_dispatch_owner(db, issue, config.implementer)?;
    let gate_commands = approved_gate_commands(db, &issue.repo)?;
    // Report the effective bound, including a supervised CLI harness's mandatory finite fallback.
    // Claude retains the repo's optional bound.
    let wall_clock_timeout_secs = gate_config::wall_clock_timeout(db, &issue.repo)?
        .or_else(|| config.implementer.fallback_wall_clock_secs());
    let wall_clock_timeout = wall_clock_timeout_secs.map(std::time::Duration::from_secs);

    let mut status = lineage::get_issue_status(db, issue)?;
    // Rung 11's second, narrower reason to dispatch a finished issue: a
    // reviewer read its PR and asked for changes. Read once here and reused
    // for every dispatch of this run — a remediation cannot arrive mid-run,
    // because arming one requires a review of a PR this run is still writing
    // commits to.
    let remediation = remediate::active_remediation(db, issue)?;
    if let Some(existing) = &status {
        if existing.best_verdict == Some(AttemptOutcome::Success) && remediation.is_none() {
            if config.implementer.recovers_invocations() {
                // Only the state this run read; see `write_state`.
                if let Some(state) = &existing_state {
                    dispatch_state::clear_dispatch_state_if_unchanged(db, state)?;
                }
            }
            return Ok(IssueRun {
                unpriced_dispatches: 0,
                issue: issue.clone(),
                terminal: TerminalReason::AlreadySucceeded {
                    commit_sha: existing.best_commit_sha.clone(),
                },
                dispatches: Vec::new(),
                total_cost_usd: 0.0,
                cumulative_attempt_n: existing.cumulative_attempt_n,
                wall_clock_timeout_secs,
            });
        }
    }
    let mut cumulative_attempt_n = status.as_ref().map(|s| s.cumulative_attempt_n).unwrap_or(0);
    // Whether this run is a remediation's, charged against its own budget
    // (#344). Fixed for the run: a succeeded issue only reaches here with a
    // remediation in force, and a run that records a success ends on it.
    let succeeded = status
        .as_ref()
        .is_some_and(|s| s.best_verdict == Some(AttemptOutcome::Success));

    // Resume the issue's existing session if one is in flight; otherwise open
    // a new one. The spec's crash-safe-state section is why the session uuid
    // lives in the drawer rather than in the caller's memory: "any Lead can
    // resume any IC".
    let session_uuid = existing_state
        .as_ref()
        .map(|state| state.session_uuid.clone())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    // A drawer existing is *not* proof the session exists: crash safety
    // makes this record be written before the first launch, so a run that
    // stopped before dispatching anything (daily budget, or repeated launch
    // failures) leaves a uuid that no `claude` process has ever seen.
    // Resuming that uuid fails on every subsequent pass, forever — so trust
    // the recorded claim, not the drawer's mere presence.
    // Mutable because a concluded goal loop rotates it; see the rotation in
    // the dispatch loop below.
    let mut session_uuid = session_uuid;
    let mut native_session_id = existing_state
        .as_ref()
        .and_then(|s| s.native_session_id.clone());
    let mut invocation_artifacts = existing_state
        .as_ref()
        .and_then(|s| s.invocation_artifacts.clone());
    // The model `session_uuid` was last dispatched with, and how many
    // finished invocations in a row the harness refused to run on it. See
    // `Implementer::rotates_refused_session` and the rotations below.
    let mut session_model = existing_state
        .as_ref()
        .map_or_else(|| Some(config.model.clone()), |s| s.session_model.clone());
    let mut resume_refusals = existing_state.as_ref().map_or(0, |s| s.resume_refusals);
    // A session the harness last reported still in use. Only the first
    // launch of this run can be on it; that launch probes it before anything
    // replaces it (see the model rotation below).
    let parked_in_use = existing_state
        .as_ref()
        .is_some_and(|s| s.state == "paused-session-in-use");
    let mut recovered_outcome = None;
    // A policy that cannot be read stops the run here, before recovery or
    // any dispatch: the guard would refuse every `met` as an infrastructure
    // failure, so a dispatch would be paid for and then discarded. So does a
    // flag that holds the branch while the policy is off. Nothing is
    // written, so the next run proceeds once the value is fixed.
    let policy_at_start = worktree::signing_policy(&worktree.path).map_err(|why| {
        MemoryError::Validation(format!(
            "commit.gpgsign cannot be read in {}, so the commit-signing guard could not check \
             any result; fix it (`git config --bool commit.gpgsign` must print true or false) \
             and run again; {}. git said: {why}",
            worktree.path.display(),
            kept_at_start(config.implementer, existing_state.as_ref())
        ))
    })?;
    if !policy_at_start {
        worktree::signing_holder(&worktree.path, &worktree.branch).map_err(|why| {
            MemoryError::Validation(format!(
                "{why} in {}, so the commit-signing guard could not check any result; fix it \
                 (it must read true or false) and run again; {}",
                worktree.path.display(),
                kept_at_start(config.implementer, existing_state.as_ref())
            ))
        })?;
    }
    let seen = SigningPolicySeen {
        at_run_start: policy_at_start,
        by_interrupted_run: existing_state.as_ref().is_some_and(|s| s.signing_held),
    };
    // A collaborator's unsigned commit already on the branch would turn away
    // any `met` this run produced: refuse before recovery or any dispatch,
    // like an unreadable policy, until an operator deals with it. Anything
    // else the guard finds now is for the dispatch to fix, or to judge again
    // when its `met` arrives.
    if let Some(fault) = collaborator_check_at_start(worktree, seen.fresh()) {
        return Err(MemoryError::Validation(format!(
            "{fault}; the run does not start, since any met it produced would be refused the \
             same way; {}",
            kept_at_start(config.implementer, existing_state.as_ref())
        )));
    }
    // Set by a guard fault every later `met` would meet too: the run stops
    // after recording it, before paying for another dispatch.
    let mut guard_stop: Option<GuardFault> = None;
    if let Some(state) = existing_state
        .as_ref()
        .filter(|_| config.implementer.recovers_invocations())
    {
        match recover_invocation(
            db,
            issue,
            worktree,
            dispatcher,
            state,
            cumulative_attempt_n,
            wall_clock_timeout_secs,
            &mut native_session_id,
            &mut invocation_artifacts,
        )? {
            Recovered::Done(run) => return Ok(run),
            Recovered::Continue(outcome) => recovered_outcome = outcome,
        }
    }
    // Record a signing policy that is on before any dispatch, so an agent
    // that turns it off is still held to it, in this run and every later one.
    // After recovery, never before: a run that holds a live invocation ends
    // above, and writing `.git/config` while that agent may be writing it too
    // would race it for the lock. A policy a crashed dispatch disabled before
    // any run or `ensure_worktree` saw it on is not caught: an accepted
    // limitation. A failed write (another worktree's agent holding the lock)
    // costs only this cross-run memory: the reading is held in memory for
    // this run and in the dispatch state written before each launch
    // (`signing_held`), which a replacement controller reads.
    if policy_at_start {
        if let Err(e) = worktree::record_signing(&worktree.path, &worktree.branch, true) {
            tracing::warn!(
                error = %e,
                branch = %worktree.branch,
                "autopilot could not record the signing policy on the branch; \
                 holding this run to it from memory"
            );
        }
    }
    let mut resuming = existing_state
        .as_ref()
        .is_some_and(|state| state.session_claimed);
    let started_at = existing_state
        .as_ref()
        .map(|state| state.started_at.clone())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
    let mut turn_n = existing_state
        .as_ref()
        .map(|state| state.turn_n)
        .unwrap_or(0);
    let ic_session_name = dispatch::ic_name(issue);
    let worktree_path = worktree.path.to_string_lossy().to_string();

    // `session_uuid` is a parameter rather than a capture because the session
    // is no longer fixed for the life of a run: a dispatch that concludes its
    // goal loop rotates it (see the rotation below), and the drawer must
    // record the uuid the *next* dispatch will use, not the spent one.
    //
    // Every write lands only while the drawer still holds `owned`, the state
    // this run last wrote (at first, the one it read): a compare-and-set, not
    // a blind upsert. Otherwise another run took the issue over, and the
    // write fails with `taken_over`. Launching then would put two
    // implementers in one worktree with one named by no state, and any write
    // at all would overwrite the state that names the other run's
    // invocation, releasing its slot while it may still run.
    let taken_over = || {
        MemoryError::Validation(format!(
            "another run took over {} after this one started (its dispatch state changed), so \
             this run launches and writes nothing more; the other run's state is left as it is",
            issue.canonical()
        ))
    };
    let write_state = |owned: &mut Option<DispatchState>,
                       attempt_n: u32,
                       turn_n: u32,
                       state: &str,
                       session_uuid: &str,
                       session_claimed: bool,
                       identity: (&Option<String>, &Option<InvocationArtifacts>),
                       session: (&Option<String>, u32)|
     -> Result<(), MemoryError> {
        let next = DispatchState {
            implementer: config.implementer,
            native_session_id: if config.implementer.assigns_own_session_id() {
                identity.0.clone()
            } else {
                Some(session_uuid.to_string())
            },
            invocation_artifacts: identity.1.clone(),
            issue: issue.clone(),
            worktree_path: worktree_path.clone(),
            ic_session_name: ic_session_name.clone(),
            dispatch_class: config.dispatch_class.clone(),
            attempt_n,
            state: state.to_string(),
            started_at: started_at.clone(),
            session_uuid: session_uuid.to_string(),
            turn_n,
            session_claimed,
            session_model: session.0.clone(),
            resume_refusals: session.1,
            // What this run saw, never a carried-forward older reading.
            signing_held: policy_at_start,
        };
        if !dispatch_state::upsert_dispatch_state_if_unchanged(db, owned.as_ref(), &next)? {
            return Err(taken_over());
        }
        *owned = Some(next);
        Ok(())
    };
    let mut owned = existing_state.clone();

    // A recovering harness publishes each invocation just before it launches.
    if !config.implementer.recovers_invocations() {
        write_state(
            &mut owned,
            cumulative_attempt_n,
            turn_n,
            "dispatching",
            &session_uuid,
            resuming,
            (&native_session_id, &invocation_artifacts),
            (&session_model, resume_refusals),
        )?;
    }

    let mut dispatches = Vec::new();
    let mut total_cost_usd = 0.0;
    let mut unpriced_dispatches = 0u32;
    let mut consecutive_infrastructure_failures = 0u32;

    let terminal = loop {
        if recovered_outcome.is_none() {
            // Rung 7's escalation, enforced rather than merely reported. Checked
            // inside the loop, not once before it: a supervisor polling
            // alongside a long run can escalate mid-run, and the next dispatch
            // must see it.
            if let Some(signature) = supervise::escalated_signature(db, issue)? {
                break TerminalReason::StrategyEscalated { signature };
            }
            // Read fresh each pass, like the escalation above it: a human who
            // grants a retry while a run is in flight must be honoured by the
            // very next dispatch, not the next run.
            if remediate::attempts_charged(db, issue, cumulative_attempt_n, succeeded)?
                >= config.attempt_cap
            {
                let attempts = prior_attempts(db, issue)?;
                record_terminal_summary(
                    db,
                    issue,
                    cumulative_attempt_n,
                    config.attempt_cap,
                    &attempts,
                )?;
                break TerminalReason::AttemptCapExhausted;
            }

            let ledger = budget::get_daily_spend(db, &today_utc())?;
            let spent_today = ledger.as_ref().map(|e| e.total_cost_usd).unwrap_or(0.0);
            if spent_today + config.max_budget_usd > config.daily_budget_usd {
                break TerminalReason::DailyBudgetExhausted {
                    spent_usd: spent_today,
                };
            }
            // Claude keeps its existing policy: this cap gates only repos
            // with a wall-clock bound, and a timed-out dispatch is banked
            // unpriced after it ends. For a supervised CLI harness (always
            // bounded) this is only a cheap pre-filter that spares rendering,
            // argv checks and an invocation directory on a capped day; the
            // transactional reservation below is the authority, since
            // another Lead can take the last unit after this read.
            if wall_clock_timeout.is_some() {
                let unpriced_today = ledger
                    .as_ref()
                    .map(|e| e.unpriced_dispatch_count)
                    .unwrap_or(0);
                if unpriced_today >= config.max_unpriced_dispatches_per_day {
                    break TerminalReason::UnpricedDispatchesExhausted { unpriced_today };
                }
            }
        }
        let recovered = recovered_outcome.is_some();
        let outcome_result = if let Some(outcome) = recovered_outcome.take() {
            Ok(outcome)
        } else {
            let attempts = prior_attempts(db, issue)?;
            // Rung 2 provisioned this slot and hardcoded it to `None`; rung 7's
            // strategy-health check is what finally fills it. Read fresh on every
            // pass so a redirect issued by a supervisor running *between*
            // dispatches (the spec's per-dispatch cadence) reaches the very next
            // one.
            let strategy_redirect = supervise::active_redirect(db, issue)?;
            // Rung 8's other half of the blocked round trip. Read fresh on every
            // pass for the same reason the redirect is: an answer that arrives
            // between two dispatches must reach the very next one, and the
            // resuming session is the one that asked the question.
            let human_answers: Vec<(String, String)> = blocked::active_answers(db, issue)?
                .into_iter()
                .filter_map(|pair| pair.answer.map(|answer| (pair.question, answer)))
                .collect();
            // Rung 11's delivery half, and the reason a remediation is more than
            // a label flip: without it the IC would be re-dispatched onto an
            // issue it has already finished, told nothing about why, and handed a
            // goal condition its own last commit already satisfies.
            let remediation_brief = remediation.as_ref().map(|r| turn_prompt::RemediationBrief {
                pr_number: r.pr_number,
                head_sha: &r.head_sha,
                findings: r.findings.as_deref(),
            });
            // A condition the platform rejects is a dispatch that silently does
            // not happen: no turn, no verdict, nothing written to the issue, and
            // `agent:ready` still set so the next tick repeats it. Refuse here,
            // before the attempt is spent — the same posture as `config.validate`
            // refusing an `--n-turns`/`--max-turns` pair at config time.
            let condition = match turn_prompt::render(&turn_prompt::TurnPromptInputs {
                issue,
                issue_title: &brief.title,
                issue_body: &brief.body,
                prior_attempts: &attempts,
                strategy_redirect: strategy_redirect.as_deref(),
                human_answers: &human_answers,
                remediation: remediation_brief,
                // The worktree's own branch, not `worktree::branch_name(issue)`
                // re-derived: `ensure_worktree` is the thing that guarantees the
                // checkout is on that branch, and it is the value it hands back
                // that the IC will actually be committing onto.
                branch: &worktree.branch,
                gate_commands: &gate_commands,
                n_turns: config.n_turns,
            }) {
                Ok(condition) => condition,
                Err(e) => {
                    // Leave the drawer telling the truth on the way out. The
                    // state written before the loop says "dispatching", and
                    // `queue::plan_queue` reads drawers, not labels: a drawer
                    // left mid-dispatch holds a concurrency slot *and* sorts its
                    // issue ahead of every new one as an in-flight resume. A
                    // refusal that never clears would hold both for ever under a
                    // state that never happened.
                    write_state(
                        &mut owned,
                        cumulative_attempt_n,
                        turn_n,
                        "paused-condition-too-long",
                        &session_uuid,
                        resuming,
                        (&native_session_id, &invocation_artifacts),
                        (&session_model, resume_refusals),
                    )?;
                    return Err(MemoryError::Validation(e.to_string()));
                }
            };

            // A session the harness keeps but refuses across providers is
            // replaced when the model changes. Here, at a fresh launch, no
            // invocation can be live: recovery has already held or finished
            // any earlier one, and every in-run outcome is a finished one.
            //
            // Not a session parked as still in use: something may still hold
            // it, and a fresh session would start a second implementer over
            // the holder. It is resumed first so the harness's own lock
            // answers. Still held, the run parks again; gone, with the
            // provider really changed, the harness refuses to resume it and
            // the usual consecutive refusals rotate it.
            if config.implementer.rotates_refused_session()
                && !parked_in_use
                && session_model
                    .as_deref()
                    .is_some_and(|model| model != config.model)
            {
                tracing::info!(
                    issue = %issue.canonical(),
                    previous_model = session_model.as_deref().unwrap_or_default(),
                    model = %config.model,
                    "autopilot opens a fresh {} session for a different model",
                    config.implementer.label()
                );
                session_uuid = uuid::Uuid::new_v4().to_string();
                resuming = false;
                resume_refusals = 0;
            }
            session_model = Some(config.model.clone());
            // A supervised harness's artifacts are allocated below, once its
            // argv is known to be valid.
            let mut spec = DispatchSpec {
                implementer: config.implementer,
                invocation_artifacts: if config.implementer.recovers_invocations() {
                    None
                } else {
                    invocation_artifacts.clone()
                },
                session: if config.implementer.assigns_own_session_id() {
                    match &native_session_id {
                        Some(id) => SessionMode::Resume {
                            session_uuid: id.clone(),
                        },
                        None => SessionMode::New {
                            session_uuid: String::new(),
                        },
                    }
                } else if resuming {
                    SessionMode::Resume {
                        session_uuid: session_uuid.clone(),
                    }
                } else {
                    SessionMode::New {
                        session_uuid: session_uuid.clone(),
                    }
                },
                name: ic_session_name.clone(),
                model: config.model.clone(),
                max_budget_usd: config.max_budget_usd,
                max_turns: config.max_turns,
                condition,
                wall_clock_timeout,
            };

            if config.implementer.recovers_invocations() {
                // Refuse a definite input error before allocating an
                // invocation directory: a Lead repeating the refusal every
                // tick would otherwise leak one per attempt.
                super::invocation::validate_argv_before_artifacts(&worktree.path, &spec)?;
                spec.invocation_artifacts =
                    Some(super::invocation::prepare_artifacts(&worktree.path, issue)?);
                // Validate the exact backend argv before publishing a new
                // invocation or charging unknown spend. Definite input errors
                // prove no process launched and must leave prior state intact.
                super::invocation::validate_argv(&worktree.path, &spec)?;
                if let Some(artifacts) = &spec.invocation_artifacts {
                    // Another run that took the issue over since this one
                    // last wrote it is seen before a unit is reserved.
                    if !dispatch_state::dispatch_state_is(db, issue, owned.as_ref())? {
                        super::invocation::discard_unlaunched(artifacts);
                        return Err(taken_over());
                    }
                    // The authority on the day's cap: check and reserve in
                    // one transaction, before this invocation is published,
                    // so two Leads cannot both take the last unit. At the cap
                    // nothing launches, no unit is reserved, the prepared
                    // directory is removed, and this invocation is never
                    // published; the run then stops like any capped run,
                    // saving `paused-infrastructure-failure` with the
                    // session it would have used (a model change may have
                    // rotated it above) and no invocation. A crash after
                    // reserving but before publishing over-counts one unit,
                    // never under-counts.
                    if let budget::UnpricedReservation::CapReached { unpriced_today } =
                        budget::reserve_unpriced_dispatch_if_below(
                            db,
                            &today_utc(),
                            &artifacts.invocation_id,
                            config.max_unpriced_dispatches_per_day,
                        )?
                    {
                        super::invocation::discard_unlaunched(artifacts);
                        break TerminalReason::UnpricedDispatchesExhausted { unpriced_today };
                    }
                    invocation_artifacts = Some(artifacts.clone());
                    // Claimed only while this run still owns the issue; see
                    // `write_state`. Nothing has launched yet, so a lost claim
                    // leaves nothing behind but the reserved unit (the
                    // look before reserving makes that rare).
                    if let Err(e) = write_state(
                        &mut owned,
                        cumulative_attempt_n,
                        turn_n,
                        "dispatching",
                        &session_uuid,
                        resuming,
                        (&native_session_id, &invocation_artifacts),
                        (&session_model, resume_refusals),
                    ) {
                        super::invocation::discard_unlaunched(artifacts);
                        return Err(e);
                    }
                    unpriced_dispatches += 1;
                }
            }
            dispatcher.dispatch(&worktree.path, &spec)
        };
        let mut outcome = match outcome_result {
            Ok(outcome) => outcome,
            Err(err) => {
                if config.implementer.recovers_invocations() {
                    dispatches.push(DispatchSummary {
                        attempt_n: None,
                        classification: DispatchClassification::InfrastructureFailure,
                        total_cost_usd: None,
                        num_turns: 0,
                        verdict: None,
                        reason: Some(err.to_string()),
                    });
                    // Errors do not prove a child is dead; retain the exact invocation for recovery.
                    break TerminalReason::ImplementationInFlight {
                        detail: format!(
                            "{} dispatch could not establish completion: {err}; {}",
                            config.implementer.label(),
                            release_hint(issue, invocation_artifacts.as_ref())
                        ),
                    };
                }
                // A dispatch that produced no parseable result at all banked
                // no cost and is an infrastructure failure by definition —
                // there is no verdict to trust and no meter to read. It never
                // consumes an attempt, so only the consecutive bound stops
                // the loop here.
                //
                // The session, though, may well exist: anything but a
                // launch failure means the process ran, and `--session-id`
                // is rejected for a uuid that already exists. See the
                // [`Dispatcher`] error contract.
                if !matches!(err, MemoryError::NotFound(_)) {
                    resuming = true;
                }
                consecutive_infrastructure_failures += 1;
                dispatches.push(DispatchSummary {
                    attempt_n: None,
                    classification: DispatchClassification::InfrastructureFailure,
                    total_cost_usd: None,
                    num_turns: 0,
                    verdict: None,
                    reason: Some(err.to_string()),
                });
                write_state(
                    &mut owned,
                    cumulative_attempt_n,
                    turn_n,
                    "infrastructure-failure",
                    &session_uuid,
                    resuming,
                    (&native_session_id, &invocation_artifacts),
                    (&session_model, resume_refusals),
                )?;
                if consecutive_infrastructure_failures
                    >= config.max_consecutive_infrastructure_failures
                {
                    break TerminalReason::InfrastructureFailure {
                        consecutive: consecutive_infrastructure_failures,
                    };
                }
                continue;
            }
        };

        // The session this dispatch used exists from here on regardless of
        // how it lands: `--session-id` may only be used once, so that uuid
        // can only ever be `--resume`d again.
        resuming = true;
        if config.implementer.recovers_invocations() {
            // Its unknown spend was reserved by the run that launched it.
            if recovered {
                unpriced_dispatches += 1;
            }
            invocation_artifacts = None;
        }
        // A harness that refuses to resume the session it is handed fails
        // every later invocation the same way, at no attempt's cost. After
        // enough refusals in a row the next dispatch opens a fresh session;
        // the outcome is finished, so nothing can still be running on it.
        // A session still in use is the exception: never rotated over.
        let release = format!(
            "`ironmem autopilot clear-dispatch {} {}`",
            issue.repo, issue.number
        );
        let label = config.implementer.label();
        let mut session_in_use = None;
        let refusal_note = match outcome.session_refusal {
            Some(SessionRefusal::InUse) => {
                resume_refusals = 0;
                let note = format!(
                    "{label} reports session {session_uuid} still in use; Autopilot won't start \
                     a new session over it; inspect for a leftover {label} process, then \
                     {release}"
                );
                session_in_use = Some(note.clone());
                Some(note)
            }
            Some(SessionRefusal::Unresumable) if config.implementer.rotates_refused_session() => {
                resume_refusals += 1;
                let fallback = format!(
                    "if a fresh session is refused too, check {label}'s provider and \
                     configuration; {release} releases the issue"
                );
                Some(if resume_refusals >= RESUME_REFUSALS_BEFORE_ROTATION {
                    let refused =
                        std::mem::replace(&mut session_uuid, uuid::Uuid::new_v4().to_string());
                    resuming = false;
                    resume_refusals = 0;
                    format!(
                        "{label} refused to resume session {refused} \
                         {RESUME_REFUSALS_BEFORE_ROTATION} times in a row, so the next dispatch \
                         opens a fresh session; {fallback}"
                    )
                } else {
                    format!(
                        "{label} refused to resume its session ({resume_refusals} of \
                         {RESUME_REFUSALS_BEFORE_ROTATION} before a fresh session is opened); \
                         {fallback}"
                    )
                })
            }
            _ => {
                resume_refusals = 0;
                None
            }
        };
        if let Some(note) = refusal_note {
            outcome.reason = Some(match outcome.reason.take() {
                Some(original) if !original.trim().is_empty() => format!("{note}. {original}"),
                _ => note,
            });
        }
        // Only a harness that assigns its own session id (see
        // `Implementer::assigns_own_session_id`) is checked here: it is the
        // one whose identity the outcome, not the controller, supplies.
        if config.implementer.assigns_own_session_id() {
            // An invocation that never launched has no thread to check, and
            // its own reason (why the spawn failed) is the useful one.
            if outcome.invocation_started
                && (outcome
                    .native_session_id
                    .as_ref()
                    .is_none_or(|id| id.trim().is_empty())
                    || native_session_id
                        .as_ref()
                        .is_some_and(|id| outcome.native_session_id.as_ref() != Some(id)))
            {
                outcome.native_session_id = None;
                outcome.verdict = None;
                outcome.is_error = true;
                outcome.process_success = false;
                // Keep the invocation's own diagnosis (stderr, timeout, exit
                // code): a harness that failed before announcing its session
                // (for Codex, its `thread.started` event) — auth, config,
                // network — is explained only there.
                let identity = format!(
                    "missing or mismatched native {} session ID; persisted identity retained",
                    config.implementer.label()
                );
                outcome.reason = Some(match outcome.reason.take() {
                    Some(original) if !original.trim().is_empty() => {
                        format!("{identity}; {original}")
                    }
                    _ => identity,
                });
            } else if native_session_id.is_none() {
                native_session_id = outcome.native_session_id.clone();
            }
        }

        // ...but a session whose `/goal` loop has **concluded** cannot be
        // resumed usefully at all, so the next dispatch opens a fresh one.
        //
        // Measured on the first live run (2026-09-08, issue #285), by reading
        // the two transcripts side by side. Into a *new* session, `-p "/goal
        // …"` arrives as a plain slash command and installs the
        // session-scoped Stop hook that drives the turn loop. Into a
        // `--resume`d one it is replayed as a local command behind a "DO NOT
        // respond to these messages ... unless the user explicitly asks"
        // caveat, and arms no hook. The model correctly obeys the caveat,
        // answers "No response requested", and the process exits in about six
        // seconds having run no turns. Resuming past a verdict is therefore
        // not merely wasteful — it cannot work, because the goal loop that
        // sequences the turns is precisely what is missing.
        //
        // The verdict is the right signal because it *is* the goal loop's own
        // evaluated stop: returning one is what spends the hook. A dispatch
        // that returned none — killed on the wall-clock bound, cut off by
        // `--max-turns`, or crashed — left the hook armed mid-goal, and
        // resuming it is both correct and cheap, which is the "~5% marginal
        // cost" resume the spec chose `--resume` for. That case is the
        // majority of resumes and is deliberately left alone.
        //
        // What a fresh session loses is in-context history, not knowledge:
        // the next turn prompt carries the prior attempts and their failure
        // reasons, which is the degradation the spec's crash-safe-state
        // section already designed for ("any Lead can resume any IC").
        if config.implementer.rotates_session_after_verdict() && outcome.verdict.is_some() {
            session_uuid = uuid::Uuid::new_v4().to_string();
            resuming = false;
        }
        turn_n = turn_n.saturating_add(outcome.num_turns);

        // Bank spend before deciding anything else. Every dispatch that
        // returned a result is billed, met or not.
        //
        // An unknown meter (including a dispatch killed on its wall-clock
        // bound) belongs in the unpriced counter. No cost measurement
        // exists to bank; the day's dollar total remains a floor.
        if config.implementer.recovers_invocations() {
            // Reserved before launch; recovery and completion never bill it again.
            outcome.cost_usd = None;
        } else if outcome.cost_usd.is_none() {
            unpriced_dispatches += 1;
            budget::record_unpriced_dispatch(db, &today_utc())?;
        } else {
            budget::accumulate_daily_spend(
                db,
                &today_utc(),
                outcome.cost_usd.unwrap_or(0.0).max(0.0),
            )?;
            total_cost_usd += outcome.cost_usd.unwrap_or(0.0).max(0.0);
        }

        // Every harness: a `met` whose branch holds commits that bypassed the
        // signing policy is a failed attempt, and the reason reaches the next
        // turn as `why_failed`. Every commit the branch added since its cut is
        // checked, not just this dispatch's, so an earlier bypass cannot be
        // waved through by a later `met` that adds nothing.
        //
        // A recovered `met` may replay an attempt that is already recorded:
        // the attempt record and the status write are separate, and a crash
        // between them leaves `cumulative_attempt_n` behind, so recovery
        // replays the outcome onto the same ordinal (a met always consumes
        // one: `cumulative_attempt_n + 1`). The guard reads live git state
        // that may have changed since, and a different verdict would make
        // `record_and_advance` refuse the replay and wedge the issue. The
        // recorded verdict is authoritative instead, and so is a recorded
        // success's commit, even when it has none: the branch tip may have
        // moved since, and the stored status reports the recorded one.
        let recorded = if recovered && outcome.is_met() {
            recorded_attempt(db, issue, cumulative_attempt_n + 1)?
        } else {
            None
        };
        // `Some(commit)` once a recorded success decides the reported commit.
        let mut recorded_commit_sha: Option<Option<String>> = None;
        // A met the guard refused: its violation and the model's own reason,
        // which the lineage keeps apart (see `refused_met_record`).
        let mut refused_met: Option<(String, Option<String>)> = None;
        if let Some(recorded) = recorded {
            if recorded.verdict == AttemptOutcome::Success {
                recorded_commit_sha = Some(recorded.commit_sha);
            } else {
                outcome.verdict = Some(Verdict::NotMet);
                outcome.reason = Some(
                    recorded
                        .why_failed
                        .filter(|why| !why.trim().is_empty())
                        .unwrap_or_else(|| "recorded attempt failed".into()),
                );
            }
        } else if outcome.is_met() {
            let model_reason = |reason: String, original: Option<String>| match original {
                Some(original) if !original.trim().is_empty() => format!(
                    "{reason} (model's reason: {})",
                    prefix(&original, MODEL_REASON_CHARS)
                ),
                _ => reason,
            };
            // An interrupted run's reading holds only its own recovered
            // invocation's met.
            let seen = if recovered { seen } else { seen.fresh() };
            match signing_violation(worktree, seen) {
                Ok(None) => {}
                Ok(Some(violation)) => {
                    let original = outcome.reason.take();
                    outcome.verdict = Some(Verdict::NotMet);
                    outcome.reason = Some(model_reason(violation.to_string(), original.clone()));
                    refused_met = Some((violation.to_string(), original));
                }
                // The guard could not accept the met, but the model is not to
                // blame: an errored outcome with no verdict is an
                // infrastructure failure, which consumes no attempt and
                // escalates through the consecutive bound, or stops the run
                // when every later met would meet the same fault.
                Err(fault) => {
                    tracing::warn!(
                        error = %fault,
                        branch = %worktree.branch,
                        "autopilot's commit-signing guard did not accept the met; \
                         it is held as an infrastructure failure"
                    );
                    outcome.verdict = None;
                    outcome.is_error = true;
                    outcome.reason = Some(model_reason(
                        format!(
                            "the commit-signing guard could not check the branch, so the met \
                             was not accepted: {fault}"
                        ),
                        outcome.reason.take(),
                    ));
                    if fault.stops_run() {
                        guard_stop = Some(fault);
                    }
                }
            }
        }

        let classification = classify(&outcome);
        let attempt_n = if classification.consumes_attempt() {
            consecutive_infrastructure_failures = 0;
            cumulative_attempt_n += 1;
            Some(cumulative_attempt_n)
        } else {
            consecutive_infrastructure_failures += 1;
            None
        };

        dispatches.push(DispatchSummary {
            attempt_n,
            classification,
            total_cost_usd: outcome.cost_usd,
            num_turns: outcome.num_turns,
            verdict: outcome.verdict,
            reason: outcome.reason.clone(),
        });

        match classification {
            DispatchClassification::Met => {
                let commit_sha = recorded_commit_sha.unwrap_or_else(|| branch_tip(worktree));
                record_and_advance(
                    db,
                    &AttemptRecord {
                        issue: issue.clone(),
                        attempt_n: cumulative_attempt_n,
                        approach: approach_text(&outcome, classification),
                        verdict: AttemptOutcome::Success,
                        why_failed: None,
                        commit_sha: commit_sha.clone(),
                    },
                    status.as_ref(),
                    config
                        .implementer
                        .recovers_invocations()
                        .then_some(config.implementer),
                )?;
                break TerminalReason::Met { commit_sha };
            }
            DispatchClassification::Impossible => {
                record_and_advance(
                    db,
                    &AttemptRecord {
                        issue: issue.clone(),
                        attempt_n: cumulative_attempt_n,
                        approach: approach_text(&outcome, classification),
                        verdict: AttemptOutcome::Failed,
                        why_failed: Some(why_failed_text(&outcome, classification)),
                        commit_sha: None,
                    },
                    status.as_ref(),
                    config
                        .implementer
                        .recovers_invocations()
                        .then_some(config.implementer),
                )?;
                break TerminalReason::Impossible {
                    reason: outcome.reason.clone(),
                };
            }
            DispatchClassification::FailedAttempt => {
                let (approach, why_failed) = match refused_met.take() {
                    Some((violation, original)) => refused_met_record(&violation, original),
                    None => (
                        approach_text(&outcome, classification),
                        why_failed_text(&outcome, classification),
                    ),
                };
                record_and_advance(
                    db,
                    &AttemptRecord {
                        issue: issue.clone(),
                        attempt_n: cumulative_attempt_n,
                        approach,
                        verdict: AttemptOutcome::Failed,
                        why_failed: Some(why_failed),
                        commit_sha: None,
                    },
                    status.as_ref(),
                    config
                        .implementer
                        .recovers_invocations()
                        .then_some(config.implementer),
                )?;
                status = lineage::get_issue_status(db, issue)?;
                write_state(
                    &mut owned,
                    cumulative_attempt_n,
                    turn_n,
                    "dispatching",
                    &session_uuid,
                    resuming,
                    (&native_session_id, &invocation_artifacts),
                    (&session_model, resume_refusals),
                )?;
            }
            DispatchClassification::InfrastructureFailure => {
                // A guard fault every later met would meet too: stop like the
                // run-start check, before another paid dispatch. A supervised
                // harness's drawer still names this finished invocation, so
                // the first run after the fix recovers and re-checks it;
                // Claude's records the session it claimed.
                if let Some(fault) = guard_stop.take() {
                    if !config.implementer.recovers_invocations() {
                        write_state(
                            &mut owned,
                            cumulative_attempt_n,
                            turn_n,
                            "infrastructure-failure",
                            &session_uuid,
                            resuming,
                            (&native_session_id, &invocation_artifacts),
                            (&session_model, resume_refusals),
                        )?;
                    }
                    return Err(MemoryError::Validation(format!(
                        "{fault}; the run stops rather than pay for another dispatch whose met \
                         would be refused the same way; {}",
                        kept_mid_run(config.implementer)
                    )));
                }
                // Retrying a session still in use can never succeed, so it
                // spends no more units, in this run or on a later Lead tick.
                if let Some(detail) = session_in_use {
                    break TerminalReason::SessionInUse { detail };
                }
                write_state(
                    &mut owned,
                    cumulative_attempt_n,
                    turn_n,
                    "infrastructure-failure",
                    &session_uuid,
                    resuming,
                    (&native_session_id, &invocation_artifacts),
                    (&session_model, resume_refusals),
                )?;
                if consecutive_infrastructure_failures
                    >= config.max_consecutive_infrastructure_failures
                {
                    break TerminalReason::InfrastructureFailure {
                        consecutive: consecutive_infrastructure_failures,
                    };
                }
            }
        }
    };

    if terminal.clears_dispatch_state() {
        // Only the state this run owns; one another run wrote is theirs.
        if let Some(owned) = &owned {
            dispatch_state::clear_dispatch_state_if_unchanged(db, owned)?;
        }
    } else if !matches!(terminal, TerminalReason::ImplementationInFlight { .. }) {
        let state_label = match &terminal {
            TerminalReason::DailyBudgetExhausted { .. } => "paused-daily-budget",
            TerminalReason::SessionInUse { .. } => "paused-session-in-use",
            _ => "paused-infrastructure-failure",
        };
        write_state(
            &mut owned,
            cumulative_attempt_n,
            turn_n,
            state_label,
            &session_uuid,
            resuming,
            (&native_session_id, &invocation_artifacts),
            (&session_model, resume_refusals),
        )?;
    }

    Ok(IssueRun {
        unpriced_dispatches,
        issue: issue.clone(),
        terminal,
        dispatches,
        total_cost_usd,
        cumulative_attempt_n,
        wall_clock_timeout_secs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::{retry, worktree};

    /// An invocation held only for a busy worktree finishes by itself; the
    /// hold message says so, and names `clear-dispatch` as the manual
    /// release. Any other held invocation still needs the operator.
    #[test]
    fn the_release_hint_tells_an_awaiting_quiet_hold_it_finishes_by_itself() {
        let (_dir, s) = crate::autopilot::invocation::contract_tests::setup();
        let a = s.invocation_artifacts.as_ref().unwrap();
        let other = release_hint(&issue(), Some(a));
        assert!(other.contains("once no process remains") && other.contains("clear-dispatch"));
        let worktree = tempfile::tempdir().unwrap();
        // SAFETY: geteuid has no preconditions.
        let uid = unsafe { libc::geteuid() };
        crate::autopilot::invocation::contract_tests::write_awaiting(
            a,
            uid,
            worktree.path(),
            Vec::new(),
        );
        let awaiting = release_hint(&issue(), Some(a));
        assert!(
            awaiting.contains("later Lead tick")
                && awaiting.contains("autopilot run")
                && awaiting.contains("manual release")
                && awaiting.contains("clear-dispatch ironrace/ironmem 283"),
            "{awaiting}"
        );
    }

    #[test]
    fn invalid_codex_resume_refuses_before_reservation_or_state_overwrite() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut state = pending_codex(&db, &wt, Some("invalid-native-uuid"));
        state.invocation_artifacts = None;
        state.state = "previous-safe-state".into();
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let ledger =
            budget::reserve_unpriced_dispatch(&db, &today_utc(), "prior-reservation").unwrap();
        let mut d = codex_script(vec![met()]);
        let result = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d);
        assert!(
            result.is_err(),
            "invalid native ID must refuse before dispatch: {result:?}"
        );
        assert!(d.inner.seen.is_empty());
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
            Some(state)
        );
        assert_eq!(
            budget::get_daily_spend(&db, &today_utc()).unwrap(),
            Some(ledger)
        );
    }
    #[test]
    fn held_codex_reports_only_its_existing_reservation_including_prior_days() {
        for recovery in [
            InvocationRecovery::Alive,
            InvocationRecovery::Unknown {
                reason: "unproven completion".into(),
            },
        ] {
            for reserved_date in [None, Some(today_utc()), Some("2026-01-01".into())] {
                let db = approved_db();
                let (_dir, wt) = fixture_worktree();
                let state = pending_codex(&db, &wt, None);
                let id = &state.invocation_artifacts.as_ref().unwrap().invocation_id;
                let unrelated =
                    budget::reserve_unpriced_dispatch(&db, &today_utc(), "unrelated-invocation")
                        .unwrap();
                let ledger = reserved_date
                    .as_ref()
                    .map(|date| budget::reserve_unpriced_dispatch(&db, date, id).unwrap());
                let before_today = budget::get_daily_spend(&db, &today_utc()).unwrap();
                let mut d = codex_script(vec![]);
                d.recovery = Some(recovery.clone());
                let run = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
                assert!(matches!(
                    run.terminal,
                    TerminalReason::ImplementationInFlight { .. }
                ));
                assert_eq!(
                    run.unpriced_dispatches,
                    u32::from(reserved_date.is_some()),
                    "a hold must report only a durable reservation for this invocation"
                );
                assert_eq!(run.total_cost_usd, 0.0);
                assert!(run.dispatches.is_empty());
                assert!(d.inner.seen.is_empty());
                assert_eq!(
                    dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
                    Some(state)
                );
                assert_eq!(
                    budget::get_daily_spend(&db, &today_utc()).unwrap(),
                    before_today
                );
                if let Some(date) = &reserved_date {
                    assert_eq!(budget::get_daily_spend(&db, date).unwrap(), ledger);
                } else {
                    assert_eq!(before_today, Some(unrelated));
                }
            }
        }
    }
    #[test]
    fn recovery_finishes_a_partially_recorded_attempt_without_duplicate_lineage() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        pending_codex(&db, &wt, None);
        lineage::record_attempt(
            &db,
            &AttemptRecord {
                issue: issue(),
                attempt_n: 1,
                approach: "already durable".into(),
                verdict: AttemptOutcome::Failed,
                why_failed: Some("failed".into()),
                commit_sha: None,
            },
        )
        .unwrap();
        let mut c = codex_config();
        c.attempt_cap = 1;
        let mut result = not_met();
        result.cost_usd = None;
        let mut d = codex_script(vec![]);
        d.recovery = Some(InvocationRecovery::Finished(result));
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        let attempts = lineage::attempts_for_issue(&db, &issue()).unwrap();
        assert_eq!(
            attempts
                .iter()
                .filter(|r| !is_terminal_summary(&r.approach))
                .count(),
            1,
            "the append-only write must not replay after a crash before status"
        );
        assert_eq!(
            lineage::get_issue_status(&db, &issue())
                .unwrap()
                .unwrap()
                .cumulative_attempt_n,
            1
        );
        assert_eq!(run.cumulative_attempt_n, 1);
        assert!(d.inner.seen.is_empty());
    }
    #[test]
    fn a_reported_zero_price_stays_known_and_missing_codex_id_starts_new() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut zero = met();
        zero.cost_usd = Some(0.0);
        let mut claude = ScriptedDispatcher::new(vec![Ok(zero)]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut claude).unwrap();
        assert_eq!(run.dispatches[0].total_cost_usd, Some(0.0));
        assert_eq!(run.unpriced_dispatches, 0);
        assert_eq!(
            budget::get_daily_spend(&db, &today_utc())
                .unwrap()
                .unwrap()
                .dispatch_count,
            1
        );
        let db = approved_db();
        let mut missing = no_verdict_error();
        missing.native_session_id = None;
        let mut d = codex_script(vec![missing, met()]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
        assert_eq!(run.unpriced_dispatches, 2);
        for spec in &d.inner.seen {
            assert!(
                matches!(&spec.session, SessionMode::New { session_uuid } if session_uuid.is_empty())
            );
        }
    }
    #[test]
    fn absolute_pending_artifacts_survive_actual_worktree_quarantine() {
        let db = approved_db();
        let (repo, wt) = fixture_worktree();
        // An operator checkout: the issue branch is the linked worktree's.
        git_in(&wt.path, &["checkout", "-q", "main"]);
        let roots = tempfile::tempdir().unwrap();
        let first = worktree::ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        let state = pending_codex(&db, &first, None);
        std::fs::write(first.path.join("dirty.txt"), "unfinished").unwrap();
        let fresh = worktree::ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert!(fresh.quarantined_from.is_some());
        let artifacts = state.invocation_artifacts.as_ref().unwrap();
        assert!(Path::new(&artifacts.journal_path).is_file());
        let mut d = codex_script(vec![]);
        d.recovery = Some(InvocationRecovery::Alive);
        let run = run_issue(&db, &issue(), &brief(), &fresh, &codex_config(), &mut d).unwrap();
        assert!(matches!(
            run.terminal,
            TerminalReason::ImplementationInFlight { .. }
        ));
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
            Some(state)
        );
        assert!(d.inner.seen.is_empty());
    }
    #[test]
    fn consumed_impossible_recovery_preserves_terminal_without_rebilling_or_sticking() {
        for native in [None, Some("11111111-2222-3333-4444-555555555555")] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            let state = pending_codex(&db, &wt, native);
            let ledger = budget::reserve_unpriced_dispatch(
                &db,
                &today_utc(),
                &state.invocation_artifacts.as_ref().unwrap().invocation_id,
            )
            .unwrap();
            let mut result = impossible();
            result.cost_usd = None;
            record_and_advance(
                &db,
                &AttemptRecord {
                    issue: issue(),
                    attempt_n: 1,
                    approach: approach_text(&result, DispatchClassification::Impossible),
                    verdict: AttemptOutcome::Failed,
                    why_failed: Some(why_failed_text(&result, DispatchClassification::Impossible)),
                    commit_sha: None,
                },
                None,
                Some(Implementer::Codex),
            )
            .unwrap();
            let attempts = lineage::attempts_for_issue(&db, &issue()).unwrap();
            let status = lineage::get_issue_status(&db, &issue()).unwrap();
            let mut d = codex_script(vec![met()]);
            d.recovery = Some(InvocationRecovery::Finished(result.clone()));
            let run = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
            assert_eq!(
                run.terminal,
                TerminalReason::Impossible {
                    reason: result.reason,
                }
            );
            assert_eq!(run.cumulative_attempt_n, 1);
            assert_eq!(run.unpriced_dispatches, 1);
            assert_eq!(run.total_cost_usd, 0.0);
            assert!(run.dispatches.is_empty());
            assert!(d.inner.seen.is_empty());
            assert_eq!(
                lineage::attempts_for_issue(&db, &issue()).unwrap(),
                attempts
            );
            assert_eq!(lineage::get_issue_status(&db, &issue()).unwrap(), status);
            assert_eq!(
                budget::get_daily_spend(&db, &today_utc()).unwrap(),
                Some(ledger)
            );
            assert!(dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .is_none());

            // A later explicitly requested run is fresh, not globally blocked
            // by an Impossible whose recovered dispatch state was cleared.
            let later = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
            assert!(matches!(later.terminal, TerminalReason::Met { .. }));
            assert_eq!(later.cumulative_attempt_n, 2);
            assert_eq!(d.inner.seen.len(), 1);
            assert!(matches!(d.inner.seen[0].session, SessionMode::New { .. }));
        }
    }
    #[test]
    fn consumed_nonterminal_or_untrusted_recovery_continues_the_shared_loop() {
        let mut nonzero = impossible();
        nonzero.process_success = false;
        let mut errored = impossible();
        errored.is_error = true;
        let mut killed = impossible();
        killed.timed_out = true;
        let mut unstarted = impossible();
        unstarted.invocation_started = false;
        let mut missing = impossible();
        missing.native_session_id = None;
        let mut conflicting = impossible();
        conflicting.native_session_id = Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into());
        for mut result in [
            not_met(),
            nonzero,
            errored,
            killed,
            unstarted,
            missing,
            conflicting,
        ] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            let native = "11111111-2222-3333-4444-555555555555";
            let state = pending_codex(&db, &wt, Some(native));
            budget::reserve_unpriced_dispatch(
                &db,
                &today_utc(),
                &state.invocation_artifacts.as_ref().unwrap().invocation_id,
            )
            .unwrap();
            record_and_advance(
                &db,
                &AttemptRecord {
                    issue: issue(),
                    attempt_n: 1,
                    approach: "durable failed attempt".into(),
                    verdict: AttemptOutcome::Failed,
                    why_failed: Some("not met".into()),
                    commit_sha: None,
                },
                None,
                Some(Implementer::Codex),
            )
            .unwrap();
            result.cost_usd = None;
            let mut d = codex_script(vec![met()]);
            d.recovery = Some(InvocationRecovery::Finished(result));
            let run = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
            assert!(matches!(run.terminal, TerminalReason::Met { .. }));
            assert_eq!(run.cumulative_attempt_n, 2);
            assert_eq!(d.inner.seen.len(), 1);
            assert!(
                matches!(&d.inner.seen[0].session, SessionMode::Resume { session_uuid } if session_uuid == native)
            );
            assert_eq!(lineage::attempts_for_issue(&db, &issue()).unwrap().len(), 2);
            assert_eq!(
                budget::get_daily_spend(&db, &today_utc())
                    .unwrap()
                    .unwrap()
                    .unpriced_dispatch_count,
                2
            );
        }
    }
    #[test]
    fn consumed_recovery_is_not_replayed_when_lineage_already_advanced() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let state = pending_codex(&db, &wt, None);
        lineage::upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Failed),
                best_commit_sha: None,
                cumulative_attempt_n: 1,
            },
        )
        .unwrap();
        let mut c = codex_config();
        c.attempt_cap = 1;
        let mut result = not_met();
        result.cost_usd = None;
        let mut d = codex_script(vec![]);
        d.recovery = Some(InvocationRecovery::Finished(result));
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(matches!(run.terminal, TerminalReason::AttemptCapExhausted));
        assert_eq!(
            run.cumulative_attempt_n, 1,
            "a classified artifact must not spend the attempt twice"
        );
        assert!(run.dispatches.is_empty());
        assert!(d.inner.seen.is_empty());
        assert!(state
            .invocation_artifacts
            .unwrap()
            .journal_path
            .starts_with('/'));
    }
    #[cfg(unix)]
    #[test]
    fn real_codex_backend_persists_unknown_price_and_conservative_recovery() {
        use std::os::unix::fs::PermissionsExt;
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let bin_dir = tempfile::tempdir().unwrap();
        let bin = bin_dir.path().join("stub-codex");
        std::fs::write(
            &bin,
            r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
if [ "$1" = '-o' ]; then shift; final="$1"; fi
shift
done
printf '%s\n' '{"type":"thread.started","thread_id":"11111111-2222-3333-4444-555555555555"}'
printf '%s\n' '{"type":"turn.completed","usage":{}}'
printf '%s\n' '{"verdict":"met","reason":"gate"}' > "$final"
sleep 0.5
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut d = CliDispatcher {
            implementer: Implementer::Codex,
            bin,
            shared_daemon: Some(crate::autopilot::process_tree::SharedDaemon {
                socket: wt.path.join("no-shared-daemon.sock"),
                exe: std::env::current_exe().unwrap(),
            }),
        };
        let state = pending_codex(&db, &wt, None);
        assert!(matches!(
            d.recover(&wt.path, &state).unwrap(),
            InvocationRecovery::Unknown { .. }
        ));
        std::fs::write(
            &state.invocation_artifacts.as_ref().unwrap().exit_path,
            "not-json",
        )
        .unwrap();
        assert!(matches!(
            d.recover(&wt.path, &state).unwrap(),
            InvocationRecovery::Unknown { .. }
        ));
        dispatch_state::clear_dispatch_state(&db, &issue()).unwrap();
        let run = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        assert_eq!(run.dispatches[0].total_cost_usd, None);
        assert_eq!(run.unpriced_dispatches, 1);
        assert_eq!(run.total_cost_usd, 0.0);
    }
    #[test]
    fn a_held_muse_dispatch_refuses_provisioning_in_its_own_name() {
        struct MuseHold(Option<InvocationRecovery>);
        impl Dispatcher for MuseHold {
            fn implementer(&self) -> Implementer {
                Implementer::Muse
            }
            fn recover(
                &mut self,
                _repo: &Path,
                _state: &DispatchState,
            ) -> Result<InvocationRecovery, MemoryError> {
                Ok(self.0.take().unwrap())
            }
            fn dispatch(
                &mut self,
                _repo: &Path,
                _spec: &DispatchSpec,
            ) -> Result<DispatchOutcome, MemoryError> {
                unreachable!("preflight never dispatches")
            }
        }
        let db = Database::open_in_memory().unwrap();
        dispatch_state::upsert_dispatch_state(
            &db,
            &DispatchState {
                implementer: Implementer::Muse,
                native_session_id: Some("11111111-2222-3333-4444-555555555555".into()),
                invocation_artifacts: None,
                issue: issue(),
                worktree_path: "/tmp/wt".into(),
                ic_session_name: "ic".into(),
                dispatch_class: "logic".into(),
                attempt_n: 1,
                state: "dispatching".into(),
                started_at: "2026-10-26T00:00:00Z".into(),
                session_uuid: "11111111-2222-3333-4444-555555555555".into(),
                turn_n: 1,
                session_claimed: true,
                session_model: None,
                resume_refusals: 0,
                signing_held: false,
            },
        )
        .unwrap();
        let refuse = |recovery| {
            preflight_worktree_provisioning(
                &db,
                &issue(),
                Implementer::Muse,
                &mut MuseHold(Some(recovery)),
            )
            .unwrap_err()
            .to_string()
        };
        let alive = refuse(InvocationRecovery::Alive);
        assert!(
            alive.contains("its Muse implementation is still alive"),
            "{alive}"
        );
        let unknown = refuse(InvocationRecovery::Unknown {
            reason: "ps timed out".into(),
        });
        assert!(
            unknown.contains("while Muse process completion is uncertain: ps timed out"),
            "{unknown}"
        );
    }
    struct CliScript {
        implementer: Implementer,
        inner: ScriptedDispatcher,
        recovery: Option<super::super::invocation::InvocationRecovery>,
    }
    impl Dispatcher for CliScript {
        fn implementer(&self) -> Implementer {
            self.implementer
        }
        fn recover(
            &mut self,
            _repo: &Path,
            _state: &DispatchState,
        ) -> Result<InvocationRecovery, MemoryError> {
            Ok(self
                .recovery
                .take()
                .unwrap_or(InvocationRecovery::NoPriorInvocation))
        }
        fn dispatch(
            &mut self,
            repo: &Path,
            spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            self.inner.dispatch(repo, spec)
        }
    }
    fn cli_script(implementer: Implementer, outcomes: Vec<DispatchOutcome>) -> CliScript {
        CliScript {
            implementer,
            inner: ScriptedDispatcher::new(
                outcomes
                    .into_iter()
                    .map(|mut o| {
                        o.cost_usd = None;
                        Ok(o)
                    })
                    .collect(),
            ),
            recovery: None,
        }
    }
    fn codex_script(outcomes: Vec<DispatchOutcome>) -> CliScript {
        cli_script(Implementer::Codex, outcomes)
    }
    fn muse_script(outcomes: Vec<DispatchOutcome>) -> CliScript {
        cli_script(Implementer::Muse, outcomes)
    }
    fn muse_config() -> RunConfig {
        let mut c = config();
        c.implementer = Implementer::Muse;
        c.model = "muse-wonder-1.0".into();
        c
    }
    fn codex_config() -> RunConfig {
        let mut c = config();
        c.implementer = Implementer::Codex;
        c.model = "gpt-6.1-sol".into();
        c
    }
    fn pending_codex(db: &Database, worktree: &Worktree, native: Option<&str>) -> DispatchState {
        pending_invocation(Implementer::Codex, db, worktree, native)
    }
    fn pending_muse(db: &Database, worktree: &Worktree, native: Option<&str>) -> DispatchState {
        pending_invocation(Implementer::Muse, db, worktree, native)
    }
    fn pending_invocation(
        implementer: Implementer,
        db: &Database,
        worktree: &Worktree,
        native: Option<&str>,
    ) -> DispatchState {
        let artifacts =
            super::super::invocation::prepare_artifacts(&worktree.path, &issue()).unwrap();
        let state = DispatchState {
            implementer,
            native_session_id: native.map(str::to_string),
            invocation_artifacts: Some(artifacts),
            issue: issue(),
            worktree_path: worktree.path.to_string_lossy().into_owned(),
            ic_session_name: "ic".into(),
            dispatch_class: "logic".into(),
            attempt_n: 0,
            state: "dispatching".into(),
            started_at: "now".into(),
            session_uuid: "controller-placeholder".into(),
            turn_n: 0,
            session_claimed: false,
            session_model: None,
            resume_refusals: 0,
            signing_held: false,
        };
        dispatch_state::upsert_dispatch_state(db, &state).unwrap();
        state
    }
    #[test]
    fn codex_retries_resume_native_thread_and_reserve_each_unknown_price_once() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut d = codex_script(vec![not_met(), met()]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
        assert!(matches!(d.inner.seen[0].session, SessionMode::New { .. }));
        assert!(
            matches!(&d.inner.seen[1].session, SessionMode::Resume { session_uuid } if session_uuid == "11111111-2222-3333-4444-555555555555")
        );
        assert_ne!(
            d.inner.seen[0].invocation_artifacts,
            d.inner.seen[1].invocation_artifacts
        );
        assert_eq!(run.unpriced_dispatches, 2);
        assert_eq!(run.total_cost_usd, 0.0);
        assert_eq!(run.dispatches[0].total_cost_usd, None);
        assert_eq!(run.wall_clock_timeout_secs, Some(1200));
        let ledger = budget::get_daily_spend(&db, &today_utc()).unwrap().unwrap();
        assert_eq!(ledger.unpriced_dispatch_count, 2);
        assert_eq!(ledger.reserved_unpriced_dispatch_ids.len(), 2);
        assert_eq!(ledger.dispatch_count, 0);
        assert_eq!(ledger.total_cost_usd, 0.0);
    }
    #[test]
    fn wrong_dispatcher_and_wrong_persisted_harness_refuse_without_overwrite_even_after_success() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let state = pending_codex(&db, &wt, Some("native"));
        let mut claude = ScriptedDispatcher::new(vec![]);
        assert!(run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut claude).is_err());
        assert!(run_issue(&db, &issue(), &brief(), &wt, &config(), &mut claude).is_err());
        assert!(claude.seen.is_empty());
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
            Some(state.clone())
        );
        lineage::upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: None,
                cumulative_attempt_n: 1,
            },
        )
        .unwrap();
        assert!(run_issue(&db, &issue(), &brief(), &wt, &config(), &mut claude).is_err());
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
            Some(state)
        );
    }
    #[test]
    fn codex_recovery_hold_preserves_exact_artifacts_and_never_dispatches() {
        for recovery in [
            InvocationRecovery::Alive,
            InvocationRecovery::Unknown {
                reason: "not proven dead".into(),
            },
        ] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            let state = pending_codex(&db, &wt, None);
            let mut d = codex_script(vec![]);
            d.recovery = Some(recovery);
            let run = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
            assert!(matches!(
                run.terminal,
                TerminalReason::ImplementationInFlight { .. }
            ));
            assert!(d.inner.seen.is_empty());
            assert_eq!(
                dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
                Some(state)
            );
            assert!(budget::get_daily_spend(&db, &today_utc())
                .unwrap()
                .is_none());
        }
    }
    #[test]
    fn a_live_muse_invocation_is_held_with_a_muse_message_and_never_redispatched() {
        for (recovery, expected) in [
            (
                InvocationRecovery::Alive,
                "Muse implementation is still alive",
            ),
            (
                InvocationRecovery::Unknown {
                    reason: "not proven dead".into(),
                },
                "not proven dead",
            ),
        ] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            let state = pending_muse(&db, &wt, None);
            let mut d = muse_script(vec![]);
            d.recovery = Some(recovery);
            let run = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d).unwrap();
            match run.terminal {
                TerminalReason::ImplementationInFlight { detail } => {
                    assert!(detail.contains(expected), "got: {detail}");
                    assert!(!detail.contains("Codex"), "got: {detail}");
                }
                other => panic!("expected a hold, got {other:?}"),
            }
            assert!(d.inner.seen.is_empty());
            assert_eq!(
                dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
                Some(state)
            );
        }
    }
    /// A finished Muse invocation that ended with no terminal record and a
    /// non-zero exit, with `stderr` as Muse's own complaint.
    fn muse_infra_failure(stderr: &str) -> DispatchOutcome {
        DispatchOutcome {
            cost_usd: None,
            num_turns: 0,
            duration_ms: 900,
            is_error: true,
            native_session_id: None,
            invocation_started: true,
            verdict: None,
            reason: Some(format!(
                "Muse journal never names the requested session; Muse run emitted no terminal \
                 record; Muse process exited with Some(1); stderr: {stderr}"
            )),
            process_success: false,
            timed_out: false,
            session_refusal: None,
        }
    }
    /// Measured (Muse 1.4.13 and 1.4.14): a resume on another provider.
    fn muse_refusal() -> DispatchOutcome {
        let mut outcome = muse_infra_failure(
            "failed to read retained session 0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e: session \
             0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e was created with provider echo; refusing to \
             resume with provider meta; start a new session to switch providers",
        );
        outcome.session_refusal = Some(SessionRefusal::Unresumable);
        outcome
    }
    /// Measured: a second `exec` on a session Muse still holds.
    fn muse_in_use() -> DispatchOutcome {
        let mut outcome =
            muse_infra_failure("session 0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e is already in use");
        outcome.session_refusal = Some(SessionRefusal::InUse);
        outcome
    }
    fn session_of(spec: &DispatchSpec) -> String {
        spec.session.session_uuid().to_owned()
    }

    #[test]
    fn two_muse_resume_refusals_open_a_fresh_session_for_the_next_dispatch() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut d = muse_script(vec![muse_refusal(), muse_refusal(), met()]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d).unwrap();
        assert!(
            matches!(run.terminal, TerminalReason::Met { .. }),
            "{:?}",
            run.terminal
        );
        let seen = &d.inner.seen;
        assert_eq!(
            session_of(&seen[0]),
            session_of(&seen[1]),
            "one refusal keeps it"
        );
        assert_ne!(
            session_of(&seen[1]),
            session_of(&seen[2]),
            "two refusals rotate it"
        );
        assert!(matches!(seen[2].session, SessionMode::New { .. }));
        let first = run.dispatches[0].reason.as_deref().unwrap();
        assert!(first.contains("1 of 2"), "{first}");
        let second = run.dispatches[1].reason.as_deref().unwrap();
        assert!(second.contains("fresh session"), "{second}");
        assert!(
            second.contains("ironmem autopilot clear-dispatch ironrace/ironmem 283"),
            "{second}"
        );
        assert!(
            second.contains("refusing to resume"),
            "Muse's own words stay: {second}"
        );
    }

    /// Muse's lock is the one guard against a second implementer that does
    /// not rest on Autopilot's own process evidence. "In use" after a finish
    /// means something still holds the session, so it is never rotated over,
    /// and since a retry can never succeed the run stops on the first one and
    /// parks the issue for an operator instead of spending more units.
    #[test]
    fn a_muse_session_in_use_stops_the_run_and_parks_the_issue() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut d = muse_script(vec![muse_in_use(), met()]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d).unwrap();
        assert_eq!(d.inner.seen.len(), 1, "no second dispatch in this run");
        let session = session_of(&d.inner.seen[0]);
        let hint = format!(
            "Muse reports session {session} still in use; Autopilot won't start a new session \
             over it; inspect for a leftover Muse process, then `ironmem autopilot \
             clear-dispatch ironrace/ironmem 283`"
        );
        match &run.terminal {
            TerminalReason::SessionInUse { detail } => {
                assert!(detail.contains(&hint), "{detail}")
            }
            other => panic!("expected SessionInUse, got {other:?}"),
        }
        assert_eq!(run.dispatches.len(), 1);
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::InfrastructureFailure
        );
        assert!(
            run.dispatches[0].reason.as_deref().unwrap().contains(&hint),
            "{:?}",
            run.dispatches[0].reason
        );
        assert_eq!(run.cumulative_attempt_n, 0, "no attempt is charged");
        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .unwrap();
        assert_eq!(state.state, "paused-session-in-use");
        assert_eq!(state.session_uuid, session, "never rotated over");
        assert!(state.invocation_artifacts.is_none(), "{state:?}");
        assert_eq!(state.resume_refusals, 0);
        // An operator who has dealt with the holder may run it again: the
        // same session continues, and the in-use left no refusal count.
        let mut again = muse_script(vec![muse_refusal(), met()]);
        let rerun = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut again).unwrap();
        assert!(matches!(rerun.terminal, TerminalReason::Met { .. }));
        let sessions: Vec<_> = again.inner.seen.iter().map(session_of).collect();
        assert_eq!(sessions, [session.clone(), session]);
    }

    /// A parked session may still be held, so even a different `--model`
    /// resumes it first: Muse's lock answers before any fresh session could
    /// start a second Muse over the holder. Still held, it parks again. Gone,
    /// with the provider really changed, Muse refuses to resume it and the
    /// usual two refusals rotate it.
    #[test]
    fn a_model_change_on_a_parked_in_use_session_probes_the_same_session_first() {
        let mut changed = muse_config();
        changed.model = "muse-wonder-2.0".into();
        let park = |db: &Database, wt: &Worktree| {
            let mut d = muse_script(vec![muse_in_use()]);
            run_issue(db, &issue(), &brief(), wt, &muse_config(), &mut d).unwrap();
            session_of(&d.inner.seen[0])
        };

        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let held = park(&db, &wt);
        let mut d = muse_script(vec![muse_in_use()]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &changed, &mut d).unwrap();
        assert_eq!(session_of(&d.inner.seen[0]), held, "probe the held session");
        assert!(matches!(run.terminal, TerminalReason::SessionInUse { .. }));
        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .unwrap();
        assert_eq!(
            (state.state.as_str(), state.session_uuid.as_str()),
            ("paused-session-in-use", held.as_str())
        );

        let db = approved_db();
        let held = park(&db, &wt);
        let mut d = muse_script(vec![muse_refusal(), muse_refusal(), met()]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &changed, &mut d).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        let sessions: Vec<_> = d.inner.seen.iter().map(session_of).collect();
        assert_eq!(sessions[..2], [held.clone(), held.clone()], "{sessions:?}");
        assert_ne!(sessions[2], held, "{sessions:?}");
    }

    #[test]
    fn muse_resume_refusals_are_counted_across_runs() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut c = muse_config();
        c.max_consecutive_infrastructure_failures = 1;
        let mut sessions = Vec::new();
        for outcome in [muse_refusal(), muse_refusal(), met()] {
            let mut d = muse_script(vec![outcome]);
            run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            sessions.push(session_of(&d.inner.seen[0]));
        }
        assert_eq!(sessions[0], sessions[1]);
        assert_ne!(sessions[1], sessions[2]);
    }

    #[test]
    fn an_ordinary_muse_infrastructure_failure_keeps_the_session() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let plain = || muse_infra_failure("error: provider request timed out after 60s");
        let mut d = muse_script(vec![plain(), plain(), met()]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        let sessions: Vec<_> = d.inner.seen.iter().map(session_of).collect();
        assert!(sessions.iter().all(|s| *s == sessions[0]), "{sessions:?}");
        // A refusal between two ordinary failures restarts the count.
        let db = approved_db();
        let mut d = muse_script(vec![muse_refusal(), plain(), muse_refusal(), met()]);
        run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d).unwrap();
        let sessions: Vec<_> = d.inner.seen.iter().map(session_of).collect();
        assert!(sessions.iter().all(|s| *s == sessions[0]), "{sessions:?}");
    }

    #[test]
    fn a_codex_or_claude_failure_quoting_refusal_text_never_rotates() {
        for (mut c, implementer) in [
            (codex_config(), Implementer::Codex),
            (config(), Implementer::Claude),
        ] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            c.max_consecutive_infrastructure_failures = 3;
            let mut d = cli_script(implementer, vec![muse_refusal(), muse_refusal(), met()]);
            run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            let seen = &d.inner.seen;
            assert_eq!(
                session_of(&seen[1]),
                session_of(&seen[2]),
                "{implementer:?}"
            );
        }
    }

    #[test]
    fn a_model_change_opens_a_fresh_muse_session_and_the_same_model_keeps_it() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut c = muse_config();
        c.max_consecutive_infrastructure_failures = 1;
        let plain = || muse_infra_failure("error: provider request timed out after 60s");
        let mut first = muse_script(vec![plain()]);
        run_issue(&db, &issue(), &brief(), &wt, &c, &mut first).unwrap();
        let mut same = muse_script(vec![plain()]);
        run_issue(&db, &issue(), &brief(), &wt, &c, &mut same).unwrap();
        assert_eq!(
            session_of(&first.inner.seen[0]),
            session_of(&same.inner.seen[0])
        );
        c.model = "muse-wonder-2.0".into();
        let mut changed = muse_script(vec![plain()]);
        run_issue(&db, &issue(), &brief(), &wt, &c, &mut changed).unwrap();
        assert_ne!(
            session_of(&same.inner.seen[0]),
            session_of(&changed.inner.seen[0])
        );
        assert!(matches!(
            changed.inner.seen[0].session,
            SessionMode::New { .. }
        ));
        // The new session is now the new model's, so the next run keeps it.
        let mut again = muse_script(vec![met()]);
        run_issue(&db, &issue(), &brief(), &wt, &c, &mut again).unwrap();
        assert_eq!(
            session_of(&changed.inner.seen[0]),
            session_of(&again.inner.seen[0])
        );
    }

    #[test]
    fn a_model_change_never_rotates_a_session_whose_invocation_may_be_live() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut c = muse_config();
        c.max_consecutive_infrastructure_failures = 1;
        let mut first = muse_script(vec![muse_infra_failure("error: timed out")]);
        run_issue(&db, &issue(), &brief(), &wt, &c, &mut first).unwrap();
        let stored = dispatch_state::get_dispatch_state(&db, &issue()).unwrap();
        c.model = "muse-wonder-2.0".into();
        let mut live = muse_script(vec![met()]);
        live.recovery = Some(InvocationRecovery::Alive);
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut live).unwrap();
        assert!(matches!(
            run.terminal,
            TerminalReason::ImplementationInFlight { .. }
        ));
        assert!(live.inner.seen.is_empty());
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
            stored
        );
    }

    #[test]
    fn a_dispatch_that_cannot_establish_completion_names_its_harness() {
        for (implementer, label) in [(Implementer::Muse, "Muse"), (Implementer::Codex, "Codex")] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            let mut d = cli_script(implementer, vec![]);
            d.inner = ScriptedDispatcher::new(vec![Err(MemoryError::Validation("boom".into()))]);
            let mut c = config();
            c.implementer = implementer;
            c.model = if implementer == Implementer::Muse {
                "muse-wonder-1.0"
            } else {
                "gpt-6.1-sol"
            }
            .into();
            let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            match run.terminal {
                TerminalReason::ImplementationInFlight { detail } => {
                    assert!(
                        detail.starts_with(&format!(
                            "{label} dispatch could not establish completion"
                        )),
                        "got: {detail}"
                    );
                }
                other => panic!("expected a hold, got {other:?}"),
            }
        }
    }
    #[test]
    fn a_recovered_ordinal_conflict_names_its_harness() {
        let record = |verdict| AttemptRecord {
            issue: issue(),
            attempt_n: 1,
            approach: "durable".into(),
            verdict,
            why_failed: None,
            commit_sha: None,
        };
        for (harness, label) in [(Implementer::Muse, "Muse"), (Implementer::Codex, "Codex")] {
            let db = Database::open_in_memory().unwrap();
            record_and_advance(&db, &record(AttemptOutcome::Failed), None, Some(harness)).unwrap();
            let err =
                record_and_advance(&db, &record(AttemptOutcome::Success), None, Some(harness))
                    .unwrap_err()
                    .to_string();
            assert!(
                err.contains(&format!(
                    "recovered {label} verdict conflicts with the already recorded attempt ordinal"
                )),
                "got: {err}"
            );
        }
    }
    #[test]
    fn recovered_success_at_cap_is_consumed_without_new_call_or_billing() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let state = pending_codex(&db, &wt, None);
        let c = codex_config();
        for n in 0..c.max_unpriced_dispatches_per_day {
            budget::reserve_unpriced_dispatch(&db, &today_utc(), &format!("other-{n}")).unwrap();
        }
        let mut result = met();
        result.cost_usd = None;
        let mut d = codex_script(vec![]);
        d.recovery = Some(InvocationRecovery::Finished(result));
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        assert!(d.inner.seen.is_empty());
        assert_eq!(run.unpriced_dispatches, 1);
        assert_eq!(run.total_cost_usd, 0.0);
        assert_eq!(
            budget::get_daily_spend(&db, &today_utc())
                .unwrap()
                .unwrap()
                .unpriced_dispatch_count,
            c.max_unpriced_dispatches_per_day
        );
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let again = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(matches!(
            again.terminal,
            TerminalReason::AlreadySucceeded { .. }
        ));
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
    }
    /// The cap is decided where the invocation is reserved, in the same
    /// transaction, so a full day launches nothing and leaves nothing behind:
    /// no prepared invocation directory, no artifacts in the stored state,
    /// and the ledger exactly as it was.
    #[test]
    fn a_supervised_dispatch_at_the_cap_launches_nothing_and_leaves_nothing_half_written() {
        for c in [codex_config(), muse_config()] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            for n in 0..c.max_unpriced_dispatches_per_day {
                budget::reserve_unpriced_dispatch(&db, &today_utc(), &format!("other-{n}"))
                    .unwrap();
            }
            let before = budget::get_daily_spend(&db, &today_utc()).unwrap();
            let mut d = cli_script(c.implementer, vec![met()]);
            let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            assert_eq!(
                run.terminal,
                TerminalReason::UnpricedDispatchesExhausted {
                    unpriced_today: c.max_unpriced_dispatches_per_day
                },
                "{:?}",
                c.implementer
            );
            assert!(d.inner.seen.is_empty(), "nothing may launch at the cap");
            assert_eq!(run.unpriced_dispatches, 0);
            assert_eq!(budget::get_daily_spend(&db, &today_utc()).unwrap(), before);
            let invocations = wt.path.join(".git/ironmem/autopilot").join(issue().slug());
            let left: Vec<_> = std::fs::read_dir(&invocations)
                .map(|dir| dir.flatten().map(|e| e.path()).collect())
                .unwrap_or_default();
            assert!(left.is_empty(), "prepared and left behind: {left:?}");
            let state = dispatch_state::get_dispatch_state(&db, &issue()).unwrap();
            assert!(
                state
                    .as_ref()
                    .is_none_or(|s| s.invocation_artifacts.is_none()),
                "{state:?}"
            );
        }
    }
    /// The cheap check before anything is rendered or allocated: a run that
    /// starts at the cap never even creates its issue's invocation directory.
    #[test]
    fn a_supervised_run_already_at_the_cap_allocates_nothing() {
        for c in [codex_config(), muse_config()] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            for n in 0..c.max_unpriced_dispatches_per_day {
                budget::reserve_unpriced_dispatch(&db, &today_utc(), &format!("other-{n}"))
                    .unwrap();
            }
            let mut d = cli_script(c.implementer, vec![met()]);
            let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            assert_eq!(
                run.terminal,
                TerminalReason::UnpricedDispatchesExhausted {
                    unpriced_today: c.max_unpriced_dispatches_per_day
                }
            );
            let issue_dir = wt.path.join(".git/ironmem/autopilot").join(issue().slug());
            assert!(
                !issue_dir.exists(),
                "{:?} allocated {issue_dir:?}",
                c.implementer
            );
        }
    }

    #[test]
    fn codex_cap_applies_without_repo_timeout_and_failed_outputs_are_reserved() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut c = codex_config();
        c.max_unpriced_dispatches_per_day = 3;
        let mut malformed = no_verdict_error();
        malformed.process_success = true;
        let mut nonzero = met();
        nonzero.process_success = false;
        nonzero.verdict = None;
        let mut d = codex_script(vec![malformed, nonzero, timed_out()]);
        c.max_consecutive_infrastructure_failures = 4;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(matches!(
            run.terminal,
            TerminalReason::UnpricedDispatchesExhausted { unpriced_today: 3 }
        ));
        assert_eq!(d.inner.seen.len(), 3);
        assert_eq!(run.unpriced_dispatches, 3);
        assert_eq!(
            budget::get_daily_spend(&db, &today_utc())
                .unwrap()
                .unwrap()
                .reserved_unpriced_dispatch_ids
                .len(),
            3
        );
        assert_eq!(run.total_cost_usd, 0.0);
    }
    #[test]
    fn codex_recovered_missing_or_conflicting_identity_cannot_succeed_or_replace_id() {
        for native in [None, Some("different")] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            pending_codex(&db, &wt, Some("persisted"));
            let mut result = met();
            result.native_session_id = native.map(str::to_string);
            result.cost_usd = None;
            let mut d = codex_script(vec![]);
            d.recovery = Some(InvocationRecovery::Finished(result));
            let mut c = codex_config();
            c.max_consecutive_infrastructure_failures = 1;
            let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            assert!(matches!(
                run.terminal,
                TerminalReason::InfrastructureFailure { .. }
            ));
            assert!(d.inner.seen.is_empty());
            assert!(
                run.dispatches[0]
                    .reason
                    .as_deref()
                    .unwrap()
                    .contains("missing or mismatched native Codex session ID"),
                "got: {:?}",
                run.dispatches[0].reason
            );
            let state = dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .unwrap();
            assert_eq!(state.native_session_id.as_deref(), Some("persisted"));
            assert!(state.invocation_artifacts.is_none());
        }
    }
    /// A dispatcher that replays a scripted list of outcomes, recording the
    /// specs it was handed. This is what makes the loop's *policy* testable:
    /// every branch below is exercised against a real database and real
    /// lineage/budget/dispatch-state writes, with only the `claude`
    /// subprocess replaced.
    struct ScriptedDispatcher {
        outcomes: std::collections::VecDeque<Result<DispatchOutcome, MemoryError>>,
        seen: Vec<DispatchSpec>,
    }

    impl ScriptedDispatcher {
        fn new(outcomes: Vec<Result<DispatchOutcome, MemoryError>>) -> Self {
            Self {
                outcomes: outcomes.into(),
                seen: Vec::new(),
            }
        }
    }

    impl Dispatcher for ScriptedDispatcher {
        fn dispatch(
            &mut self,
            _repo: &Path,
            spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            self.seen.push(spec.clone());
            self.outcomes.pop_front().unwrap_or_else(|| {
                panic!(
                    "run_issue dispatched more times than the script allowed ({} so far)",
                    self.seen.len()
                )
            })
        }
    }

    fn outcome(
        verdict: Option<Verdict>,
        is_error: bool,
        process_success: bool,
        cost: f64,
    ) -> DispatchOutcome {
        DispatchOutcome {
            cost_usd: Some(cost),
            num_turns: 3,
            duration_ms: 1_000,
            is_error,
            native_session_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
            invocation_started: true,
            verdict,
            reason: verdict.map(|v| format!("scripted {v:?}")),
            process_success,
            timed_out: false,
            session_refusal: None,
        }
    }

    fn met() -> DispatchOutcome {
        outcome(Some(Verdict::Met), false, true, 0.20)
    }

    fn not_met() -> DispatchOutcome {
        outcome(Some(Verdict::NotMet), false, true, 0.20)
    }

    fn impossible() -> DispatchOutcome {
        outcome(Some(Verdict::Impossible), false, true, 0.20)
    }

    /// Rung 2's measured shape for a `--max-turns` cut-off: errored, and no
    /// `structured_output` at all.
    fn no_verdict_error() -> DispatchOutcome {
        outcome(None, true, true, 0.20)
    }

    fn issue() -> IssueRef {
        IssueRef::new("ironrace/ironmem", 283)
    }

    fn brief() -> IssueBrief {
        IssueBrief {
            title: "Make the gate pass".to_string(),
            body: "The suite is red.".to_string(),
        }
    }

    fn config() -> RunConfig {
        RunConfig::new("claude-sonnet-5", "logic")
    }

    #[test]
    fn codex_implementer_rejects_a_claude_model_name() {
        let mut config = config();
        config.implementer = Implementer::Codex;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("Codex"), "error must name the harness: {err}");
        assert!(
            err.contains("claude-sonnet-5"),
            "error must name the incompatible model: {err}"
        );
    }

    #[test]
    fn claude_implementer_rejects_a_gpt_model_name() {
        let config = RunConfig::new("gpt-6.1-sol", "logic");
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("Claude"), "error must name the harness: {err}");
        assert!(
            err.contains("gpt-6.1-sol"),
            "error must name the incompatible model: {err}"
        );
    }

    #[test]
    fn muse_refuses_a_model_its_argv_cannot_carry() {
        for model in ["muse wonder", "muse\twonder", "-muse", "--yolo"] {
            let err = validate_implementation_model(Implementer::Muse, model)
                .unwrap_err()
                .to_string();
            assert!(err.contains("Muse implementer"), "{model:?}: {err}");
            assert!(
                err.contains("whitespace") && err.contains("'-'"),
                "{model:?}: {err}"
            );
        }
    }

    #[test]
    fn a_muse_worktree_root_with_whitespace_is_refused() {
        let err = validate_worktree_root(Implementer::Muse, Path::new("/tmp/work trees"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Muse") && err.contains("whitespace"), "{err}");
        validate_worktree_root(Implementer::Muse, Path::new("/tmp/worktrees")).unwrap();
        // Only Muse carries the workspace as a marker word.
        for implementer in [Implementer::Claude, Implementer::Codex] {
            validate_worktree_root(implementer, Path::new("/tmp/work trees")).unwrap();
        }
    }

    #[test]
    fn a_relative_muse_worktree_root_is_read_against_the_working_directory() {
        // `--worktree-root wt` from a checkout under whitespace: the root
        // does not exist yet, so only the working directory reveals it.
        let err = validate_worktree_root_from(
            Implementer::Muse,
            Path::new("wt"),
            Path::new("/nonexistent/My Work/repo"),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("whitespace"), "{err}");
        assert!(err.contains("My Work"), "{err}");
        validate_worktree_root_from(
            Implementer::Muse,
            Path::new("wt"),
            Path::new("/nonexistent/work/repo"),
        )
        .unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_muse_worktree_root_that_resolves_under_whitespace_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("my disk").join("wt");
        std::fs::create_dir_all(&target).unwrap();
        let link = dir.path().join("wt-link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(!link.to_str().unwrap().contains(' '));
        let err = validate_worktree_root(Implementer::Muse, &link)
            .unwrap_err()
            .to_string();
        assert!(err.contains("my disk"), "{err}");
    }

    #[test]
    fn a_cli_dispatcher_refuses_a_spec_or_state_for_another_harness() {
        let (_dir, wt) = fixture_worktree();
        let mut codex = CliDispatcher {
            implementer: Implementer::Codex,
            bin: wt.path.join("no-such-codex"),
            shared_daemon: None,
        };
        let spec = DispatchSpec {
            implementer: Implementer::Muse,
            invocation_artifacts: None,
            session: SessionMode::New {
                session_uuid: "11111111-2222-3333-4444-555555555555".into(),
            },
            name: "ic".into(),
            model: "muse-wonder-1.0".into(),
            max_budget_usd: 1.0,
            max_turns: 10,
            condition: "do it".into(),
            wall_clock_timeout: None,
        };
        let err = codex.dispatch(&wt.path, &spec).unwrap_err().to_string();
        assert!(err.contains("harness mismatch"), "{err}");
        assert!(err.contains("Muse") && err.contains("Codex"), "{err}");
        let state = pending_muse(&approved_db(), &wt, None);
        let err = codex.recover(&wt.path, &state).unwrap_err().to_string();
        assert!(err.contains("harness mismatch"), "{err}");
    }

    #[test]
    fn a_supervised_launcher_is_never_found_through_a_relative_path_entry() {
        let planted = tempfile::tempdir().unwrap();
        let real = tempfile::tempdir().unwrap();
        for dir in [planted.path(), real.path()] {
            let bin = dir.join("muse");
            std::fs::write(&bin, "#!/bin/sh\nsleep 1\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }
        // `planted`, spelled relative to this process's directory, comes
        // first: a repository file a relative PATH entry would reach.
        let cwd = std::env::current_dir().unwrap();
        let up = "../".repeat(cwd.components().count().saturating_sub(1));
        let relative = Path::new(&up).join(planted.path().strip_prefix("/").unwrap());
        assert!(relative.join("muse").exists());
        let path_var = std::env::join_paths([relative.as_path(), real.path()]).unwrap();
        let found = find_launcher("muse", &path_var).unwrap();
        assert_eq!(found, real.path().join("muse"));
        assert!(found.is_absolute());
    }

    #[test]
    fn an_unloadable_config_names_the_daemon_socket_lookup() {
        let err = shared_daemon_socket(Err(MemoryError::Validation(
            "unknown embed mode 'fast'".into(),
        )))
        .unwrap_err()
        .to_string();
        assert!(err.contains("shared ironmem daemon socket"), "{err}");
        assert!(err.contains("unknown embed mode 'fast'"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_muse_worktree_root_is_refused() {
        use std::os::unix::ffi::OsStrExt;
        let root = Path::new(std::ffi::OsStr::from_bytes(b"/nonexistent/wt-\xff"));
        let err = validate_worktree_root(Implementer::Muse, root)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not UTF-8"), "{err}");
    }

    #[test]
    fn a_muse_argv_refusal_creates_no_invocation_artifacts() {
        let db = approved_db();
        let (dir, repo) = fixture_worktree();
        // A linked worktree under whitespace: the common Git directory, where
        // artifacts live, is clean, but Muse's --workspace is not.
        let path = dir.path().join("has space").join("wt");
        git_in(&repo.path, &["checkout", "-q", "main"]);
        git_in(
            &repo.path,
            &["worktree", "add", path.to_str().unwrap(), &repo.branch],
        );
        let wt = Worktree {
            path: path.clone(),
            ..repo.clone()
        };
        let mut d = muse_script(vec![met()]);
        let err = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d)
            .unwrap_err()
            .to_string();
        assert!(err.contains("workspace"), "{err}");
        assert!(d.inner.seen.is_empty());
        assert!(
            !repo.path.join(".git/ironmem").exists(),
            "an argv refusal must precede artifact allocation"
        );
    }

    #[test]
    fn muse_rejects_cross_harness_model_prefixes_and_names_its_budget_flag() {
        for model in ["claude-sonnet-5", "gpt-6.1-sol"] {
            let err = validate_implementation_model(Implementer::Muse, model)
                .unwrap_err()
                .to_string();
            assert!(err.contains("Muse implementer"), "got: {err}");
        }
        validate_implementation_model(Implementer::Muse, "muse-wonder-1.0").unwrap();
        for implementer in [Implementer::Claude, Implementer::Codex] {
            let err = validate_implementation_model(implementer, "muse-wonder-1.0")
                .unwrap_err()
                .to_string();
            assert!(
                err.contains(&format!("{} implementer", implementer.label())),
                "got: {err}"
            );
        }
        let mut config = RunConfig::new("muse-wonder-1.0", "routine");
        config.implementer = Implementer::Muse;
        config.daily_budget_usd = config.max_budget_usd / 2.0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("every Muse dispatch reserves"), "got: {err}");
        assert!(err.contains("--daily-budget-usd"), "got: {err}");
    }

    #[test]
    fn empty_model_error_names_the_selected_implementer() {
        for (implementer, name) in [
            (Implementer::Claude, "Claude"),
            (Implementer::Codex, "Codex"),
        ] {
            let mut config = RunConfig::new("  ", "logic");
            config.implementer = implementer;
            let err = config.validate().unwrap_err().to_string();
            assert!(err.contains(name), "got: {err}");
            assert!(err.contains("model"), "got: {err}");
        }
    }

    #[test]
    fn model_surrounding_whitespace_is_rejected_before_prefix_compatibility() {
        for (implementer, model, name) in [
            (Implementer::Claude, " gpt-6.1-sol", "Claude"),
            (Implementer::Claude, "gpt-6.1-sol ", "Claude"),
            (Implementer::Codex, " claude-sonnet-5", "Codex"),
            (Implementer::Codex, "claude-sonnet-5 ", "Codex"),
            (Implementer::Claude, " opus", "Claude"),
            (Implementer::Codex, "gpt-custom ", "Codex"),
        ] {
            let mut config = RunConfig::new(model, "logic");
            config.implementer = implementer;
            let err = config.validate().unwrap_err().to_string();
            assert!(err.contains(name), "got: {err}");
            assert!(err.contains("whitespace"), "got: {err}");
        }
    }

    #[test]
    fn model_aliases_without_surrounding_whitespace_remain_valid() {
        for (implementer, model) in [
            (Implementer::Claude, "opus"),
            (Implementer::Claude, "custom-model"),
            (Implementer::Codex, "gpt-6.1-sol"),
            (Implementer::Codex, "custom-model"),
        ] {
            let mut config = RunConfig::new(model, "logic");
            config.implementer = implementer;
            config.validate().unwrap();
        }
    }

    fn approved_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        gate_config::propose_gate_config(
            &db,
            &issue().repo,
            vec!["cargo test --workspace".to_string()],
            Vec::new(),
        )
        .unwrap();
        gate_config::approve_gate_config(&db, &issue().repo).unwrap();
        db
    }

    /// Rung 7: a dispatch killed on the repo's wall-clock bound. No result
    /// JSON survives such a process, so every field but `timed_out` is a
    /// placeholder — `cost_usd: None` means unknown spend.
    fn timed_out() -> DispatchOutcome {
        DispatchOutcome {
            cost_usd: None,
            num_turns: 0,
            duration_ms: 600_000,
            is_error: true,
            native_session_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
            invocation_started: true,
            verdict: None,
            reason: Some("dispatch exceeded this repo's wall-clock bound".to_string()),
            process_success: false,
            timed_out: true,
            session_refusal: None,
        }
    }

    /// The measured shape of a `--resume` that no-ops, from the first live
    /// run (2026-09-08, issue #285): resuming a session whose `/goal` loop
    /// had **already concluded** exchanged one "continue" prompt for one "no
    /// response requested" and exited in about six seconds. Exit zero, no
    /// error flag, no verdict, no turns, no spend — indistinguishable from
    /// the spec's anti-stall row on every field except the two that say it
    /// never started.
    fn resume_noop() -> DispatchOutcome {
        DispatchOutcome {
            cost_usd: Some(0.0),
            num_turns: 0,
            duration_ms: 6_000,
            is_error: false,
            native_session_id: Some("11111111-2222-3333-4444-555555555555".to_string()),
            invocation_started: true,
            verdict: None,
            reason: None,
            process_success: true,
            timed_out: false,
            session_refusal: None,
        }
    }

    // ── rung 7: the wall-clock bound and the strategy redirect ──────────

    #[test]
    fn a_repo_with_no_wall_clock_bound_dispatches_unbounded_and_says_so() {
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();

        assert_eq!(run.wall_clock_timeout_secs, None);
        assert_eq!(dispatcher.seen[0].wall_clock_timeout, None);
    }

    #[test]
    fn an_approved_repos_wall_clock_bound_reaches_the_dispatch_spec() {
        let db = approved_db();
        gate_config::set_wall_clock_timeout(&db, &issue().repo, Some(1_800)).unwrap();
        let (_dir, worktree) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();

        assert_eq!(run.wall_clock_timeout_secs, Some(1_800));
        assert_eq!(
            dispatcher.seen[0].wall_clock_timeout,
            Some(std::time::Duration::from_secs(1_800))
        );
    }

    #[test]
    fn a_timed_out_dispatch_banks_unpriced_spend_never_zero_dollars() {
        // The rung-5 lesson, applied to the one dispatch outcome that has no
        // meter at all: `$0.00` would make the ledger read as a total when
        // it is only a floor.
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(timed_out()), Ok(met())]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();

        let ledger = budget::get_daily_spend(&db, &today_utc()).unwrap().unwrap();
        assert_eq!(
            ledger.unpriced_dispatch_count, 1,
            "the killed dispatch must be marked unpriced"
        );
        // Only the successful dispatch's real cost is in the total.
        assert!((ledger.total_cost_usd - 0.20).abs() < 1e-9);
        assert!((run.total_cost_usd - 0.20).abs() < 1e-9);
    }

    #[test]
    fn a_timed_out_dispatch_does_not_consume_an_attempt() {
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(timed_out()), Ok(met())]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();

        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::InfrastructureFailure
        );
        assert_eq!(run.dispatches[0].attempt_n, None);
        assert_eq!(
            run.cumulative_attempt_n, 1,
            "only the real attempt counts against the cap"
        );
    }

    #[test]
    fn repeated_timeouts_stop_the_run_rather_than_looping_forever() {
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        let mut dispatcher =
            ScriptedDispatcher::new(vec![Ok(timed_out()), Ok(timed_out()), Ok(timed_out())]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();

        assert!(matches!(
            run.terminal,
            TerminalReason::InfrastructureFailure { consecutive: 3 }
        ));
        // "Paused, not finished": the session is kept so a human who raises
        // the bound can resume it rather than starting over.
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
    }

    #[test]
    fn timed_out_dispatches_are_bounded_by_a_ceiling_the_dollar_gate_cannot_see() {
        // Rung 5's finding, carried over rather than re-learned: a timed-out
        // dispatch's spend never reaches `total_cost_usd`, so the daily
        // dollar gate is blind to it. Without this second ceiling the loop
        // would keep launching $2.50 dispatches the ledger reports as free.
        let db = approved_db();
        gate_config::set_wall_clock_timeout(&db, &issue().repo, Some(60)).unwrap();
        let (_dir, worktree) = fixture_worktree();
        let mut config = config();
        config.max_unpriced_dispatches_per_day = 2;

        let mut dispatcher =
            ScriptedDispatcher::new(vec![Ok(timed_out()), Ok(timed_out()), Ok(met())]);
        let run = run_issue(&db, &issue(), &brief(), &worktree, &config, &mut dispatcher).unwrap();

        assert!(
            matches!(
                run.terminal,
                TerminalReason::UnpricedDispatchesExhausted { unpriced_today: 2 }
            ),
            "got {:?}",
            run.terminal
        );
        assert_eq!(
            run.dispatches.len(),
            2,
            "the third dispatch must never launch — the scripted `met()` is left unused"
        );
        // Paused, not finished: the ledger rolls over at midnight.
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
    }

    #[test]
    fn a_repo_without_a_wall_clock_bound_is_not_stopped_by_other_work_unpriced_spend() {
        // A repo with no bound cannot produce a timed-out dispatch, so
        // unpriced dispatches banked elsewhere — another repo's timeouts,
        // which share this counter — must not stop it. Reviews bank to a
        // separate counter and never touch this one.
        let db = approved_db();
        for _ in 0..10 {
            budget::record_unpriced_dispatch(&db, &today_utc()).unwrap();
        }
        let (_dir, worktree) = fixture_worktree();
        let mut config = config();
        config.max_unpriced_dispatches_per_day = 2;

        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(&db, &issue(), &brief(), &worktree, &config, &mut dispatcher).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
    }

    #[test]
    fn twenty_unpriced_reviews_do_not_stop_an_ic_dispatch_with_a_wall_clock_bound() {
        let db = approved_db();
        gate_config::set_wall_clock_timeout(&db, &issue().repo, Some(60)).unwrap();
        for _ in 0..20 {
            budget::record_unpriced_review(&db, &today_utc()).unwrap();
        }
        let (_dir, worktree) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();
        assert!(
            matches!(run.terminal, TerminalReason::Met { .. }),
            "got {:?}",
            run.terminal
        );
    }

    #[test]
    fn a_zero_unpriced_ceiling_is_refused() {
        let mut config = config();
        config.max_unpriced_dispatches_per_day = 0;
        assert!(config.validate().is_err());
    }

    #[test]
    fn an_escalated_issue_is_not_dispatched_again() {
        // Rung 7's escalation is a stop, not a report. Without this the
        // issue would keep spending its remaining attempts on an approach
        // supervision already judged doomed — "never silent infinite retry",
        // silently retried.
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        supervise::upsert_supervision(
            &db,
            &supervise::SupervisionRecord {
                issue: issue(),
                fingerprint: "f".to_string(),
                progress_observed_at: chrono::Utc::now().to_rfc3339(),
                first_absent_at: None,
                last_checked_at: chrono::Utc::now().to_rfc3339(),
                active_redirect: None,
                redirect_signature: None,
                redirect_issued_after_attempts: None,
                escalated_signature: Some("the same failure".to_string()),
                redirect_proposal: None,
                escalation_notified_signature: None,
                escalation_question: None,
            },
        )
        .unwrap();

        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();

        assert!(
            matches!(run.terminal, TerminalReason::StrategyEscalated { .. }),
            "got {:?}",
            run.terminal
        );
        assert!(
            run.dispatches.is_empty(),
            "nothing may be dispatched; the scripted `met()` is left unused"
        );
        // Paused, not finished — a human clearing the escalation resumes the
        // session rather than starting over.
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_some());
    }

    #[test]
    fn clearing_the_escalation_lets_the_issue_run_again() {
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        supervise::upsert_supervision(
            &db,
            &supervise::SupervisionRecord {
                issue: issue(),
                fingerprint: "f".to_string(),
                progress_observed_at: chrono::Utc::now().to_rfc3339(),
                first_absent_at: None,
                last_checked_at: chrono::Utc::now().to_rfc3339(),
                active_redirect: None,
                redirect_signature: None,
                redirect_issued_after_attempts: None,
                escalated_signature: Some("the same failure".to_string()),
                redirect_proposal: None,
                escalation_notified_signature: None,
                escalation_question: None,
            },
        )
        .unwrap();
        supervise::clear_escalation(&db, &issue()).unwrap();

        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
    }

    #[test]
    fn an_active_strategy_redirect_reaches_the_next_dispatchs_condition() {
        // Rung 2 provisioned the turn prompt's `strategy_redirect` slot and
        // left it hardcoded to `None`. This is the wire finally being
        // connected.
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        supervise::upsert_supervision(
            &db,
            &supervise::SupervisionRecord {
                issue: issue(),
                fingerprint: "f".to_string(),
                progress_observed_at: chrono::Utc::now().to_rfc3339(),
                first_absent_at: None,
                last_checked_at: chrono::Utc::now().to_rfc3339(),
                active_redirect: Some("STRATEGY REDIRECT: stop doing that".to_string()),
                redirect_signature: Some("sig".to_string()),
                redirect_issued_after_attempts: None,
                escalated_signature: None,
                redirect_proposal: None,
                escalation_notified_signature: None,
                escalation_question: None,
            },
        )
        .unwrap();

        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();

        assert!(
            dispatcher.seen[0]
                .condition
                .contains("STRATEGY REDIRECT: stop doing that"),
            "the redirect must be in the rendered /goal condition, got: {}",
            dispatcher.seen[0].condition
        );
    }

    #[test]
    fn no_redirect_renders_exactly_as_before() {
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();
        assert!(!dispatcher.seen[0].condition.contains("STRATEGY REDIRECT"));
    }

    /// A worktree value pointing at a real single-commit repo, so
    /// `branch_tip` has a genuine SHA to read rather than a fabricated one.
    fn fixture_worktree() -> (tempfile::TempDir, Worktree) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        // The operator's global config signs and runs a pre-commit scanner;
        // both are pinned off so the fixture depends on neither.
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["config", "user.email", "autopilot@example.test"],
            vec!["config", "user.name", "Autopilot Test"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "core.hooksPath", "/dev/null"],
        ] {
            git_in(&path, &args);
        }
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        git_in(&path, &["add", "README.md"]);
        git_in(&path, &["commit", "-m", "seed"]);
        // On the issue branch, as `ensure_worktree` leaves it: the signing
        // guard and the recorded commit read that branch, not `HEAD`.
        let branch = worktree::branch_name(&issue());
        git_in(&path, &["checkout", "-q", "-b", &branch]);
        let wt = Worktree {
            path: path.clone(),
            branch,
            created: true,
            cut: git_in(&path, &["rev-parse", "HEAD"]),
            quarantined_from: None,
        };
        (dir, wt)
    }

    // ── commit signing guard ────────────────────────────────────────────

    /// The guard's verdict on `wt` as text, with nothing held from memory.
    fn check(wt: &Worktree) -> Result<Option<String>, String> {
        signing_violation(wt, SigningPolicySeen::default())
            .map(|violation| violation.map(|v| v.to_string()))
            .map_err(|fault| fault.to_string())
    }

    fn git_in(repo: &Path, args: &[&str]) -> String {
        let mut c = std::process::Command::new("git");
        crate::mcp::tools::scrub_git_environment(&mut c);
        let out = c.args(args).current_dir(repo).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Commit `file` in `repo` with `extra` leading git args; returns the new HEAD.
    fn commit_in(repo: &Path, file: &str, extra: &[&str]) -> String {
        std::fs::write(repo.join(file), file).unwrap();
        git_in(repo, &["add", file]);
        let mut args = extra.to_vec();
        args.extend(["commit", "-qm", file]);
        git_in(repo, &args);
        git_in(repo, &["rev-parse", "HEAD"])
    }

    /// A Claude-harness dispatcher that runs `before` in the worktree, then
    /// returns a met outcome, enough to drive the signing guard end to end.
    struct CommitThenMet<F: FnMut(&Path)>(F);
    impl<F: FnMut(&Path)> Dispatcher for CommitThenMet<F> {
        fn dispatch(
            &mut self,
            repo: &Path,
            _spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            (self.0)(repo);
            Ok(met())
        }
    }

    #[test]
    fn no_signing_policy_means_no_violation() {
        let (_dir, wt) = fixture_worktree(); // commit.gpgsign pinned false
        commit_in(&wt.path, "a", &[]);
        commit_in(&wt.path, "b", &[]);
        assert_eq!(check(&wt).unwrap(), None);
    }

    #[test]
    fn an_unsigned_commit_under_gpgsign_is_a_violation_naming_it() {
        let (_dir, wt) = fixture_worktree();
        // Turn the policy on *after* the base, as an operator's global config would.
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let bypass = commit_in(&wt.path, "b", &["-c", "commit.gpgsign=false"]);
        let violation = check(&wt).unwrap().unwrap();
        assert!(violation.contains(&bypass[..12]), "got: {violation}");
        assert!(violation.contains(&wt.cut[..12]), "got: {violation}");
        assert!(violation.contains("impossible"), "got: {violation}");
        assert!(
            violation.contains("commit.gpgsign=false"),
            "got: {violation}"
        );
    }

    #[test]
    fn a_policy_that_cannot_be_read_is_the_guards_failure_with_gits_complaint() {
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "maybe"]);
        let why = check(&wt).unwrap_err();
        assert!(why.contains("could not be read"), "got: {why}");
        assert!(why.contains("maybe"), "got: {why}");
    }

    #[test]
    fn an_unreadable_cut_is_the_guards_failure_when_the_policy_is_on() {
        let (_dir, mut wt) = fixture_worktree();
        // A pruned cut: a well-formed sha naming no object.
        wt.cut = "a".repeat(40);
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let why = check(&wt).unwrap_err();
        assert!(why.contains("signatures could not be read"), "got: {why}");
        assert!(why.contains("bad object"), "got: {why}");
    }

    /// A met whose guard could not run, under `max_consecutive = 1`.
    fn run_a_met_the_guard_cannot_check(
        break_it: impl FnOnce(&mut Worktree),
    ) -> (Database, IssueRun) {
        let db = approved_db();
        let (dir, mut wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        break_it(&mut wt);
        let mut dispatcher = CommitThenMet(|_: &Path| {});
        let mut c = config();
        c.max_consecutive_infrastructure_failures = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        drop(dir);
        (db, run)
    }

    #[test]
    fn a_guard_that_cannot_read_the_cut_costs_no_attempt_and_is_never_met() {
        let (db, run) = run_a_met_the_guard_cannot_check(|wt| wt.cut = "a".repeat(40));
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::InfrastructureFailure
        );
        assert_eq!(run.dispatches[0].attempt_n, None);
        assert_eq!(run.cumulative_attempt_n, 0);
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        assert!(reason.contains("signing guard"), "got: {reason}");
        assert!(reason.contains("bad object"), "got: {reason}");
        assert!(
            matches!(
                run.terminal,
                TerminalReason::InfrastructureFailure { consecutive: 1 }
            ),
            "{:?}",
            run.terminal
        );
        assert!(lineage::attempts_for_issue(&db, &issue())
            .unwrap()
            .is_empty());
    }

    /// A dispatcher for `implementer` that runs `before` in the worktree,
    /// then returns a met outcome, counting its dispatches.
    struct HarnessCommitThenMet<F: FnMut(&Path)> {
        implementer: Implementer,
        before: F,
        dispatched: u32,
    }
    impl<F: FnMut(&Path)> Dispatcher for HarnessCommitThenMet<F> {
        fn implementer(&self) -> Implementer {
            self.implementer
        }
        fn dispatch(
            &mut self,
            repo: &Path,
            _spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            self.dispatched += 1;
            (self.before)(repo);
            let mut met = met();
            if self.implementer.recovers_invocations() {
                met.cost_usd = None;
            }
            Ok(met)
        }
    }

    #[test]
    fn a_policy_made_unreadable_mid_run_stops_the_run_after_one_dispatch() {
        for implementer in [Implementer::Claude, Implementer::Muse] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
            // Readable at the run's start; the dispatch leaves a typo behind.
            let mut dispatcher = HarnessCommitThenMet {
                implementer,
                before: |path: &Path| {
                    git_in(path, &["config", "commit.gpgsign", "maybe"]);
                },
                dispatched: 0,
            };
            let mut c = if implementer == Implementer::Muse {
                muse_config()
            } else {
                config()
            };
            c.max_consecutive_infrastructure_failures = 3;
            let err = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher)
                .unwrap_err()
                .to_string();
            assert_eq!(dispatcher.dispatched, 1, "{implementer:?}: {err}");
            assert!(err.contains("commit.gpgsign could not be read"), "{err}");
            assert!(err.contains("maybe"), "{err}");
            if implementer == Implementer::Muse {
                assert!(
                    err.contains(
                        "the finished invocation is kept, and the first run after the fix \
                         checks its met again; do not clear-dispatch it"
                    ),
                    "{err}"
                );
            } else {
                // Claude has no invocation to recover: its session is resumed.
                assert!(!err.contains("invocation"), "{err}");
                assert!(
                    err.contains("the next run after the fix resumes it"),
                    "{err}"
                );
            }
            assert!(lineage::attempts_for_issue(&db, &issue())
                .unwrap()
                .is_empty());
            let state = dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .unwrap();
            if implementer == Implementer::Muse {
                // The finished invocation stays named, to be recovered and
                // checked again once the policy reads.
                assert!(state.invocation_artifacts.is_some(), "{state:?}");
            } else {
                assert_eq!(state.state, "infrastructure-failure", "{state:?}");
            }
        }
    }

    #[test]
    fn an_unreadable_signing_flag_is_the_guards_fault_never_the_models() {
        let (_dir, wt) = fixture_worktree(); // policy off
        let key = format!("branch.{}.ironmemSigning", wt.branch);
        git_in(&wt.path, &["config", &key, "maybe"]);
        let why = check(&wt).unwrap_err();
        assert!(why.contains(&key), "{why}");
        assert!(why.contains("maybe"), "{why}");
        // Unset on the branch, the repository flag decides, and is read too.
        git_in(&wt.path, &["config", "--unset", &key]);
        git_in(&wt.path, &["config", "ironmem.autopilotSigning", "maybe"]);
        let why = check(&wt).unwrap_err();
        assert!(why.contains("ironmem.autopilotSigning"), "{why}");
    }

    #[test]
    fn an_unreadable_signing_flag_at_the_runs_start_stops_before_any_dispatch() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree(); // policy off
        let key = format!("branch.{}.ironmemSigning", wt.branch);
        git_in(&wt.path, &["config", &key, "maybe"]);
        let mut dispatched = false;
        let mut dispatcher = CommitThenMet(|_: &Path| dispatched = true);
        let err = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher)
            .unwrap_err()
            .to_string();
        assert!(!dispatched, "{err}");
        assert!(err.contains(&key), "{err}");
        assert!(err.contains("nothing was written"), "{err}");
    }

    #[test]
    fn an_unsigned_commit_pushed_by_a_collaborator_is_never_met_nor_the_models_attempt() {
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let collaborator = [
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.email=human@example.test",
            "-c",
            "user.name=Human",
        ];
        // Pushed while a dispatch runs: no attempt, and the run stops rather
        // than pay for another dispatch.
        let db = approved_db();
        let mut theirs = String::new();
        let mut dispatcher = HarnessCommitThenMet {
            implementer: Implementer::Claude,
            before: |path: &Path| theirs = commit_in(path, "theirs", &collaborator),
            dispatched: 0,
        };
        let err = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher)
            .unwrap_err()
            .to_string();
        assert_eq!(dispatcher.dispatched, 1, "{err}");
        assert!(err.contains(&short(&theirs)), "{err}");
        // Both identities, so an operator can spot a harness committing as
        // someone else.
        assert!(err.contains("(committer human@example.test)"), "{err}");
        assert!(
            err.contains("this run commits as autopilot@example.test"),
            "{err}"
        );
        assert!(lineage::attempts_for_issue(&db, &issue())
            .unwrap()
            .is_empty());
        // Still there at the next run's start: it pays for nothing.
        let mut next = HarnessCommitThenMet {
            implementer: Implementer::Claude,
            before: |_: &Path| {},
            dispatched: 0,
        };
        let err = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut next)
            .unwrap_err()
            .to_string();
        assert_eq!(next.dispatched, 0, "{err}");
        assert!(err.contains("human@example.test"), "{err}");
        // One of the run's own beside it is the model's to fix, and both
        // are named.
        let ours = commit_in(&wt.path, "ours", &["-c", "commit.gpgsign=false"]);
        let violation = check(&wt).unwrap().unwrap();
        assert_eq!(
            blamed(&violation),
            [short(&ours), short(&theirs)].into(),
            "{violation}"
        );
    }

    /// An executable `sh` script at `path` that records it ran in `marker`.
    #[cfg(unix)]
    fn marker_script(path: &Path, marker: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::write(
            path,
            format!(
                "#!/bin/sh\ntouch '{}'\nsleep 0.5\nexit 1\n",
                marker.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn a_signature_program_set_in_the_repository_never_runs_in_the_controller() {
        let (dir, wt) = fixture_worktree();
        let script = dir.path().join(".git").join("verify");
        let marker = dir.path().join(".git").join("verify-ran");
        marker_script(&script, &marker);
        let allowed = dir.path().join(".git").join("allowed");
        std::fs::write(&allowed, "").unwrap();
        for (key, value) in [
            ("gpg.program", script.to_str().unwrap()),
            ("gpg.openpgp.program", script.to_str().unwrap()),
            ("gpg.ssh.program", script.to_str().unwrap()),
            ("gpg.x509.program", script.to_str().unwrap()),
            ("gpg.ssh.allowedSignersFile", allowed.to_str().unwrap()),
            ("commit.gpgsign", "true"),
        ] {
            git_in(&wt.path, &["config", key, value]);
        }
        for (file, armor) in [
            ("pgp", "PGP SIGNATURE"),
            ("ssh", "SSH SIGNATURE"),
            ("x509", "SIGNED MESSAGE"),
        ] {
            let plain = commit_in(&wt.path, file, &["-c", "commit.gpgsign=false"]);
            forge_signature_header(
                &wt,
                &plain,
                &format!("-----BEGIN {armor}-----\n \n AAAA\n -----END {armor}-----"),
            );
        }
        let _ = check(&wt);
        assert!(
            !marker.exists(),
            "the repository's signature program ran in the controller"
        );
    }

    #[test]
    fn a_guard_with_no_home_pins_gits_default_signature_programs() {
        // Under launchd `$HOME` can be unset, and `git config --global` then
        // exits 128: never a reason to turn every met into a guard fault,
        // nor to fall back on the repository's own programs.
        let (_dir, wt) = fixture_worktree();
        let git = GuardGit::build(
            &wt.path,
            SIGNING_GUARD_TIMEOUT,
            &["HOME", "XDG_CONFIG_HOME"],
        )
        .unwrap();
        assert!(
            git.pins.contains(&"gpg.ssh.program=ssh-keygen".to_string()),
            "{:?}",
            git.pins
        );
        assert!(
            git.pins.contains(&"gpg.program=gpg".to_string()),
            "{:?}",
            git.pins
        );
    }

    #[test]
    fn a_guard_check_shares_one_deadline_across_its_commands() {
        let (_dir, wt) = fixture_worktree();
        let git = GuardGit::new(&wt.path, std::time::Duration::from_millis(1500)).unwrap();
        let nap = ["-c", "alias.nap=!sleep 0.8", "nap"];
        git.run(&nap, None).unwrap();
        let why = git.run(&nap, None).unwrap_err();
        assert!(why.contains("did not finish"), "{why}");
    }

    #[test]
    fn a_guard_git_command_never_fetches_and_runs_without_lazy_fetch() {
        let (_dir, wt) = fixture_worktree();
        let git = GuardGit::new(&wt.path, SIGNING_GUARD_TIMEOUT).unwrap();
        let read = |args: &[&str]| {
            let out = git.run(args, None).unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        assert_eq!(read(&["config", "--get", "protocol.allow"]), "never");
        assert_eq!(
            read(&["-c", "alias.lazy=!printenv GIT_NO_LAZY_FETCH", "lazy"]),
            "1"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_guard_git_command_that_leaves_its_output_open_has_its_group_killed() {
        let (_dir, wt) = fixture_worktree();
        let git = GuardGit::new(&wt.path, std::time::Duration::from_millis(1000)).unwrap();
        // A background child holds stdout open after git exits.
        let why = git
            .run(
                &["-c", "alias.linger=!(sleep 37.13 &); exit 0", "linger"],
                None,
            )
            .unwrap_err();
        assert!(why.contains("left its output open"), "{why}");
        std::thread::sleep(std::time::Duration::from_millis(300));
        let left = std::process::Command::new("pgrep")
            .args(["-f", "sleep 37.13"])
            .output()
            .unwrap();
        assert!(
            left.stdout.is_empty(),
            "still running: {}",
            String::from_utf8_lossy(&left.stdout)
        );
    }

    #[test]
    fn a_run_start_check_of_an_unchanged_range_is_not_repeated() {
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        commit_in(&wt.path, "work", &[]);
        let checks = || START_CHECKS.with(std::cell::Cell::get);
        let seen = SigningPolicySeen::default();
        assert!(collaborator_check_at_start(&wt, seen).is_none());
        let first = checks();
        assert!(collaborator_check_at_start(&wt, seen).is_none());
        assert_eq!(checks(), first, "an unchanged range is read again");
        // Anything that moves the range is read again, and a collaborator's
        // commit is still found.
        commit_in(
            &wt.path,
            "theirs",
            &[
                "-c",
                "commit.gpgsign=false",
                "-c",
                "user.email=human@example.test",
            ],
        );
        assert!(matches!(
            collaborator_check_at_start(&wt, seen),
            Some(GuardFault::ForeignUnsigned { .. })
        ));
        assert_eq!(checks(), first + 1);
        // A found fault is never remembered as clean.
        assert!(collaborator_check_at_start(&wt, seen).is_some());
        assert_eq!(checks(), first + 2);
    }

    #[test]
    fn a_guard_git_command_that_hangs_is_killed_and_fails_closed() {
        let (_dir, wt) = fixture_worktree();
        let git = GuardGit::new(&wt.path, std::time::Duration::from_millis(500)).unwrap();
        let started = std::time::Instant::now();
        let why = git
            .run(&["-c", "alias.hang=!sleep 30", "hang"], None)
            .unwrap_err();
        assert!(why.contains("did not finish within"), "{why}");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_guard_git_command_that_succeeds_without_its_input_is_unreadable() {
        let (_dir, wt) = fixture_worktree();
        let git = GuardGit::new(&wt.path, SIGNING_GUARD_TIMEOUT).unwrap();
        // Far more than a pipe holds, to a command that reads none of it.
        let lines = vec!["a".repeat(40); 20_000];
        let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
        let why = git
            .run(&["-c", "alias.skip=!sleep 0.5", "skip"], Some(&lines))
            .unwrap_err();
        assert!(why.contains("without reading all its input"), "{why}");
    }

    #[cfg(unix)]
    #[test]
    fn one_commits_signature_read_blames_only_a_signature_git_cannot_parse() {
        use std::os::unix::process::ExitStatusExt;
        let read = |code: i32, stdout: &str, stderr: &str| {
            signature_kept(&std::process::Output {
                status: std::process::ExitStatus::from_raw(code << 8),
                stdout: stdout.as_bytes().to_vec(),
                stderr: stderr.as_bytes().to_vec(),
            })
        };
        assert_eq!(read(0, "G\n", ""), Ok(true));
        assert_eq!(read(0, "U\n", ""), Ok(true));
        assert_eq!(read(0, "B\n", ""), Ok(false));
        assert_eq!(
            read(128, "", "fatal: bad/incompatible signature 'garbage\n'\n"),
            Ok(false)
        );
        // Any other failure is the guard's, with git's complaint.
        let why = read(128, "", "fatal: bad object deadbeef").unwrap_err();
        assert!(why.contains("bad object"), "{why}");
        assert!(read(1, "", "error: something else").is_err());
    }

    #[test]
    fn the_recipe_pushes_to_a_configured_remote_never_a_lookalike_ref() {
        let (_dir, wt) = remote_fixture();
        git_in(&wt.path, &["remote", "rename", "origin", "fork"]);
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        commit_in(&wt.path, "work", &["-c", "commit.gpgsign=false"]);
        // No remote tracks the branch yet: the only remote is the one to use.
        let violation = check(&wt).unwrap().unwrap();
        assert!(
            violation.contains(&format!("`git push --force-with-lease fork {}`", wt.branch)),
            "{violation}"
        );
        // A ref that merely ends in the branch name, under no configured
        // remote, sorting first, is not the branch's remote.
        git_in(&wt.path, &["push", "-q", "fork", &wt.branch]);
        let tip = git_in(&wt.path, &["rev-parse", "HEAD"]);
        git_in(
            &wt.path,
            &[
                "update-ref",
                &format!("refs/remotes/aaa/{}", wt.branch),
                &tip,
            ],
        );
        let violation = check(&wt).unwrap().unwrap();
        assert!(
            violation.contains(&format!("`git push --force-with-lease fork {}`", wt.branch)),
            "{violation}"
        );
    }

    #[test]
    fn an_unreadable_policy_at_the_runs_start_stops_before_any_dispatch() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "maybe"]);
        let mut dispatched = false;
        let mut dispatcher = CommitThenMet(|_: &Path| dispatched = true);
        let err = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher)
            .unwrap_err()
            .to_string();
        assert!(
            !dispatched,
            "nothing may be paid for a met that would be discarded"
        );
        assert!(err.contains("commit.gpgsign"), "got: {err}");
        assert!(err.contains("maybe"), "got: {err}");
        // No invocation and no state: it says so, rather than promise a
        // recovery that cannot happen.
        assert!(!err.contains("invocation"), "got: {err}");
        assert!(err.contains("nothing was written"), "got: {err}");
        assert!(lineage::attempts_for_issue(&db, &issue())
            .unwrap()
            .is_empty());
        // A held invocation is recovered on the next run once the policy
        // reads; clearing it would discard a run that may still be live.
        let pending = pending_muse(&db, &wt, None);
        let mut d = muse_script(vec![]);
        let err = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(
                "a pending invocation for this issue is kept and will be recovered once this \
                 is fixed; do not clear-dispatch it"
            ),
            "got: {err}"
        );
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &issue()).unwrap(),
            Some(pending)
        );
        // A state with no invocation is left for the next run to resume.
        let mut paused = pending_codex(&db, &wt, None);
        paused.invocation_artifacts = None;
        dispatch_state::upsert_dispatch_state(&db, &paused).unwrap();
        let err = run_issue(
            &db,
            &issue(),
            &brief(),
            &wt,
            &codex_config(),
            &mut codex_script(vec![]),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("dispatch state is left as it is"),
            "got: {err}"
        );
    }

    #[test]
    fn only_a_full_object_name_is_read_as_a_commit_to_check() {
        assert!(is_full_object_name(&"a".repeat(40)));
        assert!(is_full_object_name(&"0F".repeat(32)));
        for line in [
            "Good \"git\" signature with ED25519 key SHA256:abc",
            "No principal matched.",
            "",
            &"a".repeat(39),
            &"g".repeat(40),
        ] {
            assert!(!is_full_object_name(line), "{line:?}");
        }
    }

    #[test]
    fn a_met_left_on_another_checkout_is_still_checked_on_the_issue_branch() {
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let bypass = commit_in(&wt.path, "b", &["-c", "commit.gpgsign=false"]);
        // The IC compares against the base and leaves HEAD there.
        git_in(&wt.path, &["checkout", "-q", "--detach", &wt.cut]);
        let violation = check(&wt).unwrap().unwrap();
        assert_eq!(blamed(&violation), [short(&bypass)].into(), "{violation}");
    }

    #[test]
    fn a_met_records_the_issue_branchs_tip_not_wherever_head_was_left() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree(); // policy off: only the sha is at issue
        let cut = wt.cut.clone();
        let mut tip = String::new();
        let mut dispatcher = CommitThenMet(|path: &Path| {
            tip = commit_in(path, "work", &[]);
            git_in(path, &["checkout", "-q", "--detach", &cut]);
        });
        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();
        match run.terminal {
            TerminalReason::Met { commit_sha } => assert_eq!(commit_sha, Some(tip)),
            other => panic!("expected Met, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_issue_branch_is_a_violation_naming_it() {
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        git_in(&wt.path, &["checkout", "-q", "--detach"]);
        git_in(&wt.path, &["branch", "-q", "-D", &wt.branch]);
        let violation = check(&wt).unwrap().unwrap();
        assert!(violation.contains(&wt.branch), "got: {violation}");
        assert!(violation.contains("does not exist"), "got: {violation}");
    }

    #[test]
    fn a_commit_only_on_the_issue_branchs_remote_is_checked_and_upstream_is_not() {
        let (dir, wt) = remote_fixture();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let pushed = commit_in(&wt.path, "pushed", &["-c", "commit.gpgsign=false"]);
        git_in(&wt.path, &["push", "-q", "origin", &wt.branch]);
        // The local branch is reset after the push; the PR still ships it.
        git_in(&wt.path, &["reset", "-q", "--hard", &wt.cut]);
        let upstream = push_upstream_commit(dir.path());
        git_in(&wt.path, &["fetch", "-q", "origin"]);
        let violation = check(&wt).unwrap().unwrap();
        assert_eq!(
            blamed(&violation),
            [short(&pushed)].into(),
            "never upstream {}: {violation}",
            short(&upstream)
        );
    }

    #[test]
    fn the_violation_names_at_most_five_commits_and_re_signs_only_those() {
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let shas: Vec<String> = (0..7)
            .map(|i| commit_in(&wt.path, &format!("c{i}"), &["-c", "commit.gpgsign=false"]))
            .collect();
        let violation = check(&wt).unwrap().unwrap();
        // Newest first: the five newest are named, the rest counted.
        for sha in &shas[2..] {
            assert!(violation.contains(&short(sha)), "{violation}");
        }
        assert!(!violation.contains(&short(&shas[1])), "{violation}");
        assert!(violation.contains("and 2 more"), "{violation}");
        // The oldest is not in the list, only in the rebase command.
        assert_eq!(
            violation.matches(&short(&shas[0])).count(),
            1,
            "{violation}"
        );
        // The rebase starts at the oldest flagged commit's parent, never the
        // cut, so upstream commits merged in are not rewritten.
        let oldest = short(&shas[0]);
        assert!(
            violation.contains(&format!("git commit --amend --no-edit -S' {oldest}^")),
            "{violation}"
        );
        assert!(
            !violation.contains(&format!("-S' {}", short(&wt.cut))),
            "{violation}"
        );
        assert!(
            violation.contains("`git commit --amend --no-edit -S`"),
            "{violation}"
        );
        assert!(violation.contains("merge"), "{violation}");
        // It acts on the issue branch, never on wherever HEAD was left, and
        // pushes it by name (no remote-tracking ref yet: `origin`).
        assert!(
            violation.contains(&format!("`git checkout {}`", wt.branch)),
            "{violation}"
        );
        assert!(
            violation.contains(&format!(
                "`git push --force-with-lease origin {}`",
                wt.branch
            )),
            "{violation}"
        );
    }

    #[test]
    fn the_recipe_pushes_to_the_remote_that_already_tracks_the_issue_branch() {
        let (_dir, wt) = remote_fixture();
        git_in(&wt.path, &["remote", "rename", "origin", "fork"]);
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        commit_in(&wt.path, "work", &["-c", "commit.gpgsign=false"]);
        git_in(&wt.path, &["push", "-q", "fork", &wt.branch]);
        let violation = check(&wt).unwrap().unwrap();
        assert!(
            violation.contains(&format!("`git push --force-with-lease fork {}`", wt.branch)),
            "{violation}"
        );
    }

    /// Insert a `gpgsig` header holding `signature` into `sha`'s object and
    /// point the issue branch at the result; returns the new commit.
    fn forge_signature_header(wt: &Worktree, sha: &str, signature: &str) -> String {
        let object = git_in(&wt.path, &["cat-file", "commit", sha]);
        let (headers, message) = object.split_once("\n\n").unwrap();
        let forged = format!("{headers}\ngpgsig {signature}\n\n{message}\n");
        let file = wt.path.join(".git").join("forged-commit");
        std::fs::write(&file, forged).unwrap();
        let commit = git_in(
            &wt.path,
            &["hash-object", "-t", "commit", "-w", file.to_str().unwrap()],
        );
        git_in(
            &wt.path,
            &["update-ref", &format!("refs/heads/{}", wt.branch), &commit],
        );
        commit
    }

    #[test]
    fn a_commit_with_a_garbage_signature_header_is_the_models_failed_attempt() {
        // Measured (git 2.55): `%G?` on such a commit exits 128 ("bad/
        // incompatible signature"); listing the range by `%H` alone does not.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let branch_wt = wt.clone();
        let mut forged = String::new();
        let mut dispatcher = CommitThenMet(|path: &Path| {
            let plain = commit_in(path, "work", &["-c", "commit.gpgsign=false"]);
            forged = forge_signature_header(&branch_wt, &plain, "garbage");
        });
        let mut c = config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        assert_eq!(blamed(reason), [short(&forged)].into(), "{reason}");
    }

    /// A fresh ed25519 key in `dir`, or `None` when `ssh-keygen` is missing,
    /// after saying so. Under `CI` a missing `ssh-keygen` fails the test
    /// instead: CI must never pass these by skipping them.
    #[cfg(unix)]
    fn ssh_key(dir: &Path) -> Option<std::path::PathBuf> {
        let key = dir.join("key");
        let made = std::process::Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key)
            .stdin(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success());
        if made {
            return Some(key);
        }
        assert!(
            std::env::var_os("CI").is_none(),
            "ssh-keygen unavailable under CI; the SSH signing tests cannot be skipped there"
        );
        eprintln!("SKIPPED: ssh-keygen unavailable");
        None
    }

    #[cfg(unix)]
    #[test]
    fn signed_commits_are_read_the_same_under_log_show_signature() {
        let keys = tempfile::tempdir().unwrap();
        let Some(key) = ssh_key(keys.path()) else {
            return;
        };
        let (_dir, wt) = fixture_worktree();
        sign_with(&wt.path, keys.path(), &key);
        // Measured (git 2.55): this prints "Good \"git\" signature ..." lines
        // ahead of each signed commit, even under `--format=%H`.
        git_in(&wt.path, &["config", "log.showSignature", "true"]);
        commit_in(&wt.path, "one", &[]);
        commit_in(&wt.path, "two", &[]);
        assert_eq!(check(&wt), Ok(None));
    }

    /// Sign in `repo` with `key`, pinned against the operator's global
    /// config: ssh-keygen as the program and a local allowed-signers file
    /// listing `key`.
    #[cfg(unix)]
    fn sign_with(repo: &Path, keys: &Path, key: &Path) {
        let public = std::fs::read_to_string(key.with_extension("pub")).unwrap();
        let allowed = keys.join("allowed");
        std::fs::write(&allowed, format!("autopilot@example.test {public}")).unwrap();
        for (k, v) in [
            ("gpg.format", "ssh"),
            ("gpg.ssh.program", "ssh-keygen"),
            ("gpg.ssh.allowedSignersFile", allowed.to_str().unwrap()),
            ("user.signingkey", key.to_str().unwrap()),
            ("commit.gpgsign", "true"),
        ] {
            git_in(repo, &["config", k, v]);
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_signed_commit_whose_signature_no_longer_verifies_is_blamed() {
        let keys = tempfile::tempdir().unwrap();
        let Some(key) = ssh_key(keys.path()) else {
            return;
        };
        let (_dir, wt) = fixture_worktree();
        sign_with(&wt.path, keys.path(), &key);
        let signed = commit_in(&wt.path, "signed", &[]);
        // Measured (git 2.55): editing the message of an SSH-signed commit
        // keeps its `gpgsig` header and reads `%G?` = `B` whatever the
        // allowed-signers file lists, so only the bad-signature arm catches it.
        let object = git_in(&wt.path, &["cat-file", "commit", &signed]);
        let tampered_text = object.replace("\n\nsigned", "\n\ntampered") + "\n";
        let file = keys.path().join("tampered");
        std::fs::write(&file, tampered_text).unwrap();
        let tampered = git_in(
            &wt.path,
            &["hash-object", "-t", "commit", "-w", file.to_str().unwrap()],
        );
        git_in(
            &wt.path,
            &[
                "update-ref",
                &format!("refs/heads/{}", wt.branch),
                &tampered,
            ],
        );
        assert_eq!(
            git_in(&wt.path, &["log", "-1", "--format=%G?", &tampered]),
            "B"
        );
        // Measured (git 2.55): with this set, `%G?` is printed after the
        // signature's own verification lines. The batched read keeps only
        // `<sha> <status>` lines; the per-commit fallback's
        // `--no-show-signature` is pinned by the mixed-range test below.
        git_in(&wt.path, &["config", "log.showSignature", "true"]);
        let violation = check(&wt).unwrap().unwrap();
        assert_eq!(blamed(&violation), [short(&tampered)].into(), "{violation}");
    }

    /// Good, unsigned, bad and unparseable signatures in one range, read
    /// under `log.showSignature=true`: the batched `%G?` read answers the
    /// first range, and the unparseable header (which fails that read with
    /// exit 128) sends the second range to one read per commit. Both blame
    /// exactly the same commits.
    #[cfg(unix)]
    #[test]
    fn a_mixed_range_is_judged_the_same_batched_or_one_commit_at_a_time() {
        let keys = tempfile::tempdir().unwrap();
        let Some(key) = ssh_key(keys.path()) else {
            return;
        };
        let (_dir, wt) = fixture_worktree();
        sign_with(&wt.path, keys.path(), &key);
        let good = commit_in(&wt.path, "good", &[]);
        let unsigned = commit_in(&wt.path, "plain", &["-c", "commit.gpgsign=false"]);
        let signed = commit_in(&wt.path, "signed", &[]);
        let object = git_in(&wt.path, &["cat-file", "commit", &signed]);
        let file = keys.path().join("tampered");
        std::fs::write(&file, object.replace("\n\nsigned", "\n\ntampered") + "\n").unwrap();
        let bad = git_in(
            &wt.path,
            &["hash-object", "-t", "commit", "-w", file.to_str().unwrap()],
        );
        git_in(&wt.path, &["reset", "-q", "--hard", &bad]);
        git_in(&wt.path, &["config", "log.showSignature", "true"]);
        let batched = check(&wt).unwrap().unwrap();
        assert_eq!(
            blamed(&batched),
            [short(&unsigned), short(&bad)].into(),
            "never the good {}: {batched}",
            short(&good)
        );
        let plain = commit_in(&wt.path, "garbled", &["-c", "commit.gpgsign=false"]);
        let garbled = forge_signature_header(&wt, &plain, "garbage");
        let one_by_one = check(&wt).unwrap().unwrap();
        assert_eq!(
            blamed(&one_by_one),
            [short(&unsigned), short(&bad), short(&garbled)].into(),
            "never the good {}: {one_by_one}",
            short(&good)
        );
    }

    /// Thousands of commits are read through pipes in both directions; a
    /// writer that fills git's input before reading its output would hang.
    #[test]
    fn a_range_of_thousands_of_commits_is_checked() {
        const COMMITS: usize = 3000;
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut script = String::new();
        for i in 0..COMMITS {
            let message = format!("bulk {i}\n");
            script.push_str(&format!(
                "commit refs/heads/{}\ncommitter Autopilot Test <autopilot@example.test> {} +0000\n\
                 data {}\n{message}",
                wt.branch,
                1_700_000_000 + i,
                message.len()
            ));
            if i == 0 {
                script.push_str(&format!("from {}\n", wt.cut));
            }
            script.push('\n');
        }
        let mut command = std::process::Command::new("git");
        crate::mcp::tools::scrub_git_environment(&mut command);
        let mut child = command
            .args(["fast-import", "--quiet"])
            .current_dir(&wt.path)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), script.as_bytes()).unwrap();
        drop(child.stdin.take());
        assert!(child.wait().unwrap().success());
        let violation = check(&wt).unwrap().unwrap();
        assert!(
            violation.contains(&format!("and {} more", COMMITS - NAMED_UNSIGNED_COMMITS)),
            "{violation}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_sha256_repositorys_gpgsig_sha256_header_counts_as_signed() {
        let keys = tempfile::tempdir().unwrap();
        let Some(key) = ssh_key(keys.path()) else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        for args in [
            vec![
                "init",
                "-q",
                "--object-format=sha256",
                "--initial-branch=main",
            ],
            vec!["config", "user.email", "autopilot@example.test"],
            vec!["config", "user.name", "Autopilot Test"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "core.hooksPath", "/dev/null"],
        ] {
            git_in(&path, &args);
        }
        let cut = commit_in(&path, "seed", &[]);
        let branch = worktree::branch_name(&issue());
        git_in(&path, &["checkout", "-q", "-b", &branch]);
        sign_with(&path, keys.path(), &key);
        let signed = commit_in(&path, "signed", &[]);
        // Measured (git 2.55): a SHA-256 repository writes the signature
        // under `gpgsig-sha256`, never `gpgsig`.
        let object = git_in(&path, &["cat-file", "commit", &signed]);
        assert!(object.contains("\ngpgsig-sha256 "), "{object}");
        assert!(!object.contains("\ngpgsig "), "{object}");
        let wt = Worktree {
            path,
            branch,
            created: true,
            cut,
            quarantined_from: None,
        };
        assert_eq!(check(&wt).unwrap(), None);
    }

    #[test]
    fn a_policy_turned_off_by_a_sibling_issues_agent_still_guards_a_new_branch() {
        let db = approved_db();
        let dir = tempfile::tempdir().unwrap();
        let checkout = operator_checkout(dir.path());
        git_in(&checkout, &["config", "commit.gpgsign", "true"]);
        let trees = dir.path().join("trees");
        let first = worktree::ensure_worktree(&checkout, &trees, &issue(), "HEAD").unwrap();
        // Issue 1's agent turns signing off in the shared `.git/config`.
        let mut off = CommitThenMet(|path: &Path| {
            git_in(path, &["config", "commit.gpgsign", "false"]);
        });
        let mut c = config();
        c.attempt_cap = 1;
        run_issue(&db, &issue(), &brief(), &first, &c, &mut off).unwrap();
        // A later issue is provisioned and run with the policy already off.
        let later = IssueRef::new("ironrace/ironmem", 3);
        let second = worktree::ensure_worktree(&checkout, &trees, &later, "HEAD").unwrap();
        let mut unsigned = CommitThenMet(|path: &Path| {
            commit_in(path, "work", &[]);
        });
        let run = run_issue(&db, &later, &brief(), &second, &c, &mut unsigned).unwrap();
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        assert!(reason.contains("was turned off"), "got: {reason}");
        assert!(
            reason.contains("`ironmem.autopilotSigning`"),
            "the repository flag holds the new branch: {reason}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn ssh_signed_commits_are_accepted_whether_or_not_the_signer_is_trusted() {
        let keys = tempfile::tempdir().unwrap();
        let Some(key) = ssh_key(keys.path()) else {
            return;
        };
        let public = std::fs::read_to_string(keys.path().join("key.pub")).unwrap();
        // A listed key typically reads `G` and an unlisted one `U`, but the
        // exact letter varies by git version; only "signed" is asserted.
        for listed in [false, true] {
            let (_dir, wt) = fixture_worktree();
            let allowed = keys.path().join(format!("allowed-{listed}"));
            let line = if listed {
                format!("autopilot@example.test {public}")
            } else {
                String::new()
            };
            std::fs::write(&allowed, line).unwrap();
            for (k, v) in [
                ("gpg.format", "ssh"),
                ("gpg.ssh.program", "ssh-keygen"),
                ("gpg.ssh.allowedSignersFile", allowed.to_str().unwrap()),
                ("user.signingkey", key.to_str().unwrap()),
                ("commit.gpgsign", "true"),
            ] {
                git_in(&wt.path, &["config", k, v]);
            }
            commit_in(&wt.path, "signed", &[]);
            let status = git_in(&wt.path, &["log", "-1", "--format=%G?"]);
            assert!(
                !status.is_empty() && status != "N" && status != "B",
                "listed={listed}: %G? = {status:?}"
            );
            assert_eq!(check(&wt).unwrap(), None, "listed={listed}");
        }
    }

    #[test]
    fn a_met_with_an_unsigned_commit_becomes_a_failed_attempt() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut dispatcher = CommitThenMet(|path: &Path| {
            commit_in(path, "work", &["-c", "commit.gpgsign=false"]);
        });
        let mut c = config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        assert!(reason.contains("unsigned"), "got: {reason}");
        // The model's own account is kept beside the violation.
        assert!(
            reason.contains("(model's reason: scripted Met)"),
            "got: {reason}"
        );
        assert!(!matches!(run.terminal, TerminalReason::Met { .. }));
    }

    /// The longest a violation may be: a third of the turn prompt's budget,
    /// so its prior-attempt line, beside the model's bounded reason, fits
    /// with the prompt's fixed text rather than being dropped.
    const MAX_VIOLATION_CHARS: usize = turn_prompt::MAX_CONDITION_CHARS / 3;

    #[test]
    fn the_longest_violation_fits_its_share_of_the_turn_prompt() {
        // GitHub's longest owner (39) and repository (100) names.
        let branch = worktree::branch_name(&IssueRef::new(
            format!("{}/{}", "o".repeat(39), "r".repeat(100)),
            u64::MAX,
        ));
        let flagged: Vec<String> = (0..1000).map(|i| format!("{i:012x}")).collect();
        let violation = SigningViolation::Unsigned {
            flagged,
            cut: "c".repeat(64),
            branch,
            remote: "r".repeat(40),
        }
        .to_string();
        assert!(
            violation.chars().count() <= MAX_VIOLATION_CHARS,
            "{} > {MAX_VIOLATION_CHARS}: {violation}",
            violation.chars().count()
        );
    }

    /// Commits unsigned work, then reports `met` with `reason`; afterwards
    /// reports `not_met`. Records every dispatch's condition.
    struct UnsignedMetThenNotMet {
        reason: Option<String>,
        commits: usize,
        conditions: Vec<String>,
    }
    impl Dispatcher for UnsignedMetThenNotMet {
        fn dispatch(
            &mut self,
            repo: &Path,
            spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            self.conditions.push(spec.condition.clone());
            if self.conditions.len() > 1 {
                return Ok(not_met());
            }
            for i in 0..self.commits {
                commit_in(repo, &format!("w{i}"), &["-c", "commit.gpgsign=false"]);
            }
            let mut met = met();
            met.reason = self.reason.clone();
            Ok(met)
        }
    }

    fn run_unsigned_met(
        reason: Option<String>,
        commits: usize,
    ) -> (Database, IssueRun, UnsignedMetThenNotMet) {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut dispatcher = UnsignedMetThenNotMet {
            reason,
            commits,
            conditions: Vec::new(),
        };
        let mut c = config();
        c.attempt_cap = 2;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        (db, run, dispatcher)
    }

    #[test]
    fn a_refused_met_tells_the_next_turn_why_once_and_bounds_the_models_reason() {
        let long = "m".repeat(2000);
        let (db, run, dispatcher) = run_unsigned_met(Some(long), 7);
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
        // The report keeps the model's reason beside the violation, bounded.
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        let quoted = format!("(model's reason: {})", "m".repeat(MODEL_REASON_CHARS));
        assert!(reason.ends_with(&quoted), "{reason}");
        // The lineage holds the violation once, in why_failed, and the
        // model's own account, bounded, as the approach.
        let attempt = &lineage::attempts_for_issue(&db, &issue()).unwrap()[0];
        assert_eq!(attempt.approach, "m".repeat(MODEL_REASON_CHARS));
        let why = attempt.why_failed.as_deref().unwrap();
        assert!(why.contains("Re-sign only these commits"), "{why}");
        assert!(!why.contains("mmmm"), "{why}");
        // ...so the next turn is told why its met was refused.
        let next = &dispatcher.conditions[1];
        assert!(next.contains("attempt 1:"), "{next}");
        assert!(next.contains("Re-sign only these commits"), "{next}");
    }

    #[test]
    fn a_refused_met_with_no_reason_of_its_own_gets_no_empty_quote() {
        for reason in [None, Some("  ".to_string())] {
            let (db, run, _) = run_unsigned_met(reason.clone(), 1);
            let report = run.dispatches[0].reason.as_deref().unwrap();
            assert!(!report.contains("model's reason"), "{reason:?}: {report}");
            let attempt = &lineage::attempts_for_issue(&db, &issue()).unwrap()[0];
            assert!(!attempt.approach.trim().is_empty(), "{reason:?}");
            assert!(!attempt.approach.contains("Re-sign"), "{attempt:?}");
        }
    }

    #[test]
    fn an_unsigned_commit_from_an_earlier_dispatch_still_fails_a_later_met() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut calls = 0;
        let mut first = String::new();
        let mut dispatcher = CommitThenMet(|path: &Path| {
            calls += 1;
            if calls == 1 {
                first = commit_in(path, "work", &["-c", "commit.gpgsign=false"]);
            }
        });
        let mut c = config();
        c.attempt_cap = 2;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        assert_eq!(run.dispatches.len(), 2);
        for d in &run.dispatches {
            assert_eq!(d.classification, DispatchClassification::FailedAttempt);
            assert!(d.reason.as_deref().unwrap().contains(&first[..12]));
        }
    }

    #[test]
    fn a_commit_already_on_the_base_is_never_blamed() {
        let db = approved_db();
        let (_dir, mut wt) = fixture_worktree();
        // The branch is cut after this unsigned commit, so it is the base's.
        wt.cut = commit_in(&wt.path, "old", &[]);
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut dispatcher = CommitThenMet(|_: &Path| {});
        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
    }

    #[test]
    fn a_dispatch_that_turns_the_policy_off_is_a_failed_attempt() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut dispatcher = CommitThenMet(|path: &Path| {
            git_in(path, &["config", "commit.gpgsign", "false"]);
            commit_in(path, "work", &[]);
        });
        let mut c = config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        assert!(reason.contains("was turned off"), "got: {reason}");
        assert!(
            reason.contains(&format!("`branch.{}.ironmemSigning`", wt.branch)),
            "the flag holding the branch is named: {reason}"
        );
        assert!(reason.contains("--unset commit.gpgsign"), "got: {reason}");
        assert!(
            reason.contains("return verdict impossible"),
            "got: {reason}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_run_whose_branch_already_records_the_policy_writes_no_config() {
        use std::os::unix::fs::MetadataExt;
        // A live agent in another worktree may be writing `.git/config`
        // (`push -u`, `checkout -b`); a needless write here races its lock.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let flag = format!("branch.{}.ironmemSigning", wt.branch);
        git_in(&wt.path, &["config", &flag, "true"]);
        git_in(&wt.path, &["config", "ironmem.autopilotSigning", "true"]);
        let config_file = wt.path.join(".git/config");
        let before = std::fs::metadata(&config_file).unwrap().ino();
        let mut dispatcher = CommitThenMet(|path: &Path| {
            commit_in(path, "work", &["-c", "commit.gpgsign=false"]);
        });
        let mut c = config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        assert_eq!(
            std::fs::metadata(&config_file).unwrap().ino(),
            before,
            "git config rewrites by rename, so a new inode means a write"
        );
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
    }

    #[test]
    fn a_held_config_lock_does_not_fail_the_run_and_the_policy_is_still_held() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        // Another worktree's agent holds the lock as the run starts.
        let lock = wt.path.join(".git/config.lock");
        std::fs::write(&lock, "").unwrap();
        let mut dispatcher = CommitThenMet(|path: &Path| {
            std::fs::remove_file(path.join(".git/config.lock")).unwrap();
            git_in(path, &["config", "commit.gpgsign", "false"]);
            commit_in(path, "work", &[]);
        });
        let mut c = config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        assert!(reason.contains("was turned off"), "got: {reason}");
        assert!(
            reason.contains("on when this run started"),
            "no flag was written, so the run's own reading holds it: {reason}"
        );
    }

    #[test]
    fn a_recovered_met_is_checked_against_the_branch_base() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        pending_codex(&db, &wt, Some("persisted"));
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        commit_in(&wt.path, "work", &["-c", "commit.gpgsign=false"]);
        let mut result = met();
        result.native_session_id = Some("persisted".into());
        let mut d = codex_script(vec![]);
        d.recovery = Some(InvocationRecovery::Finished(result));
        let mut c = codex_config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(!matches!(run.terminal, TerminalReason::Met { .. }));
        assert!(run.dispatches[0]
            .reason
            .as_deref()
            .unwrap()
            .contains("unsigned"));
    }

    #[test]
    fn a_run_holding_a_live_invocation_writes_no_signing_config() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        pending_muse(&db, &wt, None);
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut d = muse_script(vec![]);
        d.recovery = Some(InvocationRecovery::Alive);
        let run = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d).unwrap();
        assert!(
            matches!(run.terminal, TerminalReason::ImplementationInFlight { .. }),
            "{:?}",
            run.terminal
        );
        // The live agent may be writing `.git/config` itself; the run that
        // holds it must not race it for the lock.
        for key in [
            format!("branch.{}.ironmemSigning", wt.branch),
            "ironmem.autopilotSigning".to_string(),
        ] {
            let mut c = std::process::Command::new("git");
            crate::mcp::tools::scrub_git_environment(&mut c);
            let out = c
                .args(["config", "--get", &key])
                .current_dir(&wt.path)
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(1), "{key} was written");
        }
    }

    /// A paused dispatch state for `implementer`, as a run that saw the
    /// signing policy on left it, with no invocation pending.
    fn paused_state_that_saw_signing_on(
        implementer: Implementer,
        db: &Database,
        wt: &Worktree,
    ) -> DispatchState {
        let mut state = pending_invocation(implementer, db, wt, None);
        state.invocation_artifacts = None;
        state.session_uuid = "11111111-2222-3333-4444-555555555555".into();
        state.state = "paused-infrastructure-failure".into();
        state.signing_held = true;
        dispatch_state::upsert_dispatch_state(db, &state).unwrap();
        state
    }

    #[test]
    fn an_earlier_runs_reading_never_holds_a_fresh_dispatch_to_signing() {
        // Policy off, and either nothing records it on or the operator opted
        // the branch out; an earlier run on the issue saw it on.
        for (implementer, opt_out) in [
            (Implementer::Claude, false),
            (Implementer::Claude, true),
            (Implementer::Muse, false),
            (Implementer::Muse, true),
        ] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            if opt_out {
                let key = format!("branch.{}.ironmemSigning", wt.branch);
                git_in(&wt.path, &["config", &key, "false"]);
            }
            paused_state_that_saw_signing_on(implementer, &db, &wt);
            let mut d = HarnessCommitThenMet {
                implementer,
                before: |path: &Path| {
                    commit_in(path, "work", &["-c", "commit.gpgsign=false"]);
                },
                dispatched: 0,
            };
            let mut c = if implementer == Implementer::Muse {
                muse_config()
            } else {
                config()
            };
            c.attempt_cap = 1;
            let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            assert!(
                matches!(run.terminal, TerminalReason::Met { .. }),
                "{implementer:?} opt_out={opt_out}: {:?}",
                run.dispatches
            );
        }
    }

    #[test]
    fn an_explicit_opt_out_also_releases_a_recovered_met() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let key = format!("branch.{}.ironmemSigning", wt.branch);
        git_in(&wt.path, &["config", &key, "false"]);
        let mut state = pending_muse(&db, &wt, None);
        state.signing_held = true;
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        commit_in(&wt.path, "work", &["-c", "commit.gpgsign=false"]);
        let mut result = met();
        result.cost_usd = None;
        let mut d = muse_script(vec![]);
        d.recovery = Some(InvocationRecovery::Finished(result));
        let mut c = muse_config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(
            matches!(run.terminal, TerminalReason::Met { .. }),
            "{:?}",
            run.dispatches
        );
    }

    #[test]
    fn a_run_records_only_its_own_reading_of_the_signing_policy() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree(); // policy off
        paused_state_that_saw_signing_on(Implementer::Muse, &db, &wt);
        let mut d = muse_script(vec![muse_infra_failure("network down")]);
        let mut c = muse_config();
        c.max_consecutive_infrastructure_failures = 1;
        run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .unwrap();
        assert!(!state.signing_held, "{state:?}");
    }

    #[test]
    fn a_policy_seen_on_by_an_interrupted_run_still_guards_its_recovered_met() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree(); // policy off, no flag recorded
                                             // The interrupted run saw the policy on, but neither config flag was
                                             // written (another agent held the lock); only its drawer says so.
        let mut state = pending_muse(&db, &wt, None);
        state.signing_held = true;
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let mut result = met();
        result.cost_usd = None;
        let mut d = muse_script(vec![]);
        d.recovery = Some(InvocationRecovery::Finished(result));
        let mut c = muse_config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(!matches!(run.terminal, TerminalReason::Met { .. }));
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        assert!(reason.contains("interrupted run"), "{reason}");
    }

    #[test]
    fn every_published_invocation_records_whether_signing_was_seen_on() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut d = muse_script(vec![muse_infra_failure("network down")]);
        let mut c = muse_config();
        c.max_consecutive_infrastructure_failures = 1;
        run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .unwrap();
        assert!(state.signing_held, "{state:?}");
    }

    /// A dispatcher whose recovery step lets another run take the issue over:
    /// it writes `theirs` as the dispatch state, as a concurrent Lead would.
    struct TakenOver<'a> {
        db: &'a Database,
        theirs: DispatchState,
        dispatched: bool,
        recovery: Option<InvocationRecovery>,
    }
    impl Dispatcher for TakenOver<'_> {
        fn implementer(&self) -> Implementer {
            Implementer::Muse
        }
        fn recover(
            &mut self,
            _repo: &Path,
            _state: &DispatchState,
        ) -> Result<InvocationRecovery, MemoryError> {
            dispatch_state::upsert_dispatch_state(self.db, &self.theirs).unwrap();
            Ok(self
                .recovery
                .take()
                .unwrap_or(InvocationRecovery::NoPriorInvocation))
        }
        fn dispatch(
            &mut self,
            _repo: &Path,
            _spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            self.dispatched = true;
            Ok(met())
        }
    }

    #[test]
    fn a_run_whose_issue_another_run_took_over_launches_nothing_and_keeps_theirs() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut ours = pending_muse(&db, &wt, None);
        ours.invocation_artifacts = None;
        ours.session_uuid = "11111111-2222-3333-4444-555555555555".into();
        ours.state = "paused-infrastructure-failure".into();
        dispatch_state::upsert_dispatch_state(&db, &ours).unwrap();
        let before = files_under(&wt.path.join(".git/ironmem"));
        let mut theirs = ours.clone();
        theirs.session_uuid = "22222222-3333-4444-5555-666666666666".into();
        theirs.state = "dispatching".into();
        let mut d = TakenOver {
            db: &db,
            theirs: theirs.clone(),
            dispatched: false,
            recovery: None,
        };
        let err = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d)
            .unwrap_err()
            .to_string();
        assert!(!d.dispatched, "{err}");
        assert!(err.contains("another run"), "{err}");
        let stored = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .unwrap();
        assert_eq!(stored.session_uuid, theirs.session_uuid);
        assert_eq!(stored.state, "dispatching");
        // Nothing reserved for an invocation that never launched.
        let ledger = budget::get_daily_spend(&db, &today_utc()).unwrap();
        assert_eq!(ledger.map_or(0, |l| l.unpriced_dispatch_count), 0);
        // And no invocation directory left behind.
        assert_eq!(files_under(&wt.path.join(".git/ironmem")), before);
    }

    #[test]
    fn a_run_that_stops_before_launching_never_overwrites_the_state_another_run_took() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut ours = pending_muse(&db, &wt, None);
        ours.invocation_artifacts = None;
        ours.state = "paused-daily-budget".into();
        dispatch_state::upsert_dispatch_state(&db, &ours).unwrap();
        let mut theirs = ours.clone();
        theirs.state = "dispatching".into();
        theirs.session_uuid = "22222222-3333-4444-5555-666666666666".into();
        // The day is spent, so this run pauses without launching.
        budget::accumulate_daily_spend(&db, &today_utc(), 24.0).unwrap();
        let mut d = TakenOver {
            db: &db,
            theirs: theirs.clone(),
            dispatched: false,
            recovery: None,
        };
        let err = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d)
            .unwrap_err()
            .to_string();
        assert!(err.contains("another run"), "{err}");
        let stored = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .unwrap();
        assert_eq!(stored.session_uuid, theirs.session_uuid);
        assert_eq!(stored.state, "dispatching");
    }

    #[test]
    fn a_recovered_impossible_never_clears_a_state_another_run_took() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let ours = pending_muse(&db, &wt, None);
        // Its attempt was already recorded before the crash.
        lineage::upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Failed),
                best_commit_sha: None,
                cumulative_attempt_n: 1,
            },
        )
        .unwrap();
        let mut theirs = ours.clone();
        theirs.session_uuid = "22222222-3333-4444-5555-666666666666".into();
        let mut finished = impossible();
        finished.cost_usd = None;
        let mut d = TakenOver {
            db: &db,
            theirs: theirs.clone(),
            dispatched: false,
            recovery: Some(InvocationRecovery::Finished(finished)),
        };
        let run = run_issue(&db, &issue(), &brief(), &wt, &muse_config(), &mut d).unwrap();
        assert!(
            matches!(run.terminal, TerminalReason::Impossible { .. }),
            "{:?}",
            run.terminal
        );
        let stored = dispatch_state::get_dispatch_state(&db, &issue()).unwrap();
        assert_eq!(stored.map(|s| s.session_uuid), Some(theirs.session_uuid));
    }

    /// Every file path under `dir`, recursively.
    fn files_under(dir: &Path) -> std::collections::BTreeSet<std::path::PathBuf> {
        let mut found = std::collections::BTreeSet::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.is_dir() {
                found.extend(files_under(&path));
            } else {
                found.insert(path);
            }
        }
        found
    }

    /// The crash window [`a_recovered_met_is_checked_against_the_branch_base`]
    /// does not cover: the attempt record was written, the status was not, so
    /// the replay takes the same ordinal while the signing state may have
    /// changed since.
    fn record_attempt_one_before_the_crash(
        db: &Database,
        verdict: AttemptOutcome,
        why_failed: Option<&str>,
        commit_sha: Option<&str>,
    ) {
        lineage::record_attempt(
            db,
            &AttemptRecord {
                issue: issue(),
                attempt_n: 1,
                approach: "recorded before the crash".into(),
                verdict,
                why_failed: why_failed.map(str::to_string),
                commit_sha: commit_sha.map(str::to_string),
            },
        )
        .unwrap();
        assert!(
            lineage::get_issue_status(db, &issue()).unwrap().is_none(),
            "setup: the crash came before the status write"
        );
    }

    fn assert_attempt_one_recorded_once(db: &Database, verdict: AttemptOutcome) {
        let attempts: Vec<_> = lineage::attempts_for_issue(db, &issue())
            .unwrap()
            .into_iter()
            .filter(|r| !is_terminal_summary(&r.approach))
            .collect();
        assert_eq!(attempts.len(), 1, "{attempts:?}");
        assert_eq!(attempts[0].verdict, verdict);
        assert_eq!(
            lineage::get_issue_status(db, &issue())
                .unwrap()
                .unwrap()
                .cumulative_attempt_n,
            1
        );
    }

    #[test]
    fn a_recovered_met_already_recorded_as_success_skips_the_live_signing_guard() {
        // The recorded commit is reported even when HEAD has moved since, and
        // a success recorded without one reports none: never the unchecked
        // live HEAD, and always what the stored status says.
        for record_commit in [true, false] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            pending_codex(&db, &wt, Some("persisted"));
            let recorded_sha = record_commit.then(|| git_in(&wt.path, &["rev-parse", "HEAD"]));
            record_attempt_one_before_the_crash(
                &db,
                AttemptOutcome::Success,
                None,
                recorded_sha.as_deref(),
            );
            // Between the crash and the replay the policy came on and an
            // unsigned commit moved HEAD; the guard would now call the
            // recorded success a failure, and HEAD is no longer the commit
            // that succeeded.
            git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
            let later = commit_in(&wt.path, "later", &["-c", "commit.gpgsign=false"]);
            assert_ne!(Some(&later), recorded_sha.as_ref());
            let mut result = met();
            result.native_session_id = Some("persisted".into());
            let mut d = codex_script(vec![]);
            d.recovery = Some(InvocationRecovery::Finished(result));
            let mut c = codex_config();
            c.attempt_cap = 1;
            let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            match &run.terminal {
                TerminalReason::Met { commit_sha } => {
                    assert_eq!(commit_sha, &recorded_sha, "record_commit={record_commit}")
                }
                other => panic!("expected Met, got {other:?}"),
            }
            assert_eq!(
                run.dispatches[0].classification,
                DispatchClassification::Met
            );
            assert!(d.inner.seen.is_empty());
            assert_attempt_one_recorded_once(&db, AttemptOutcome::Success);
            assert_eq!(
                lineage::get_issue_status(&db, &issue())
                    .unwrap()
                    .unwrap()
                    .best_commit_sha,
                recorded_sha
            );
        }
    }

    #[test]
    fn a_recovered_met_already_recorded_as_failed_keeps_the_recorded_failure() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        pending_muse(&db, &wt, None);
        let recorded =
            "gate condition not met: commit.gpgsign is on, but commit(s) abc are unsigned";
        record_attempt_one_before_the_crash(&db, AttemptOutcome::Failed, Some(recorded), None);
        // The fixture's policy is off and no flag was recorded, so the guard
        // would now pass the met the recorded attempt failed.
        let mut d = muse_script(vec![]);
        d.recovery = Some(InvocationRecovery::Finished(met()));
        let mut c = muse_config();
        c.attempt_cap = 1;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(
            !matches!(run.terminal, TerminalReason::Met { .. }),
            "{run:?}"
        );
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
        assert_eq!(run.dispatches[0].reason.as_deref(), Some(recorded));
        assert!(d.inner.seen.is_empty());
        assert_attempt_one_recorded_once(&db, AttemptOutcome::Failed);
    }

    /// The short shas a violation blames, from its `commit(s) a, b on` list.
    fn blamed(violation: &str) -> std::collections::BTreeSet<String> {
        let list = violation
            .split_once("commit(s) ")
            .and_then(|(_, rest)| rest.split_once(" on this branch"))
            .map_or("", |(list, _)| list);
        list.split(", ").map(str::to_string).collect()
    }

    fn short(sha: &str) -> String {
        sha[..12].to_string()
    }

    /// An operator checkout under `dir`, pinned against the global config,
    /// with a bare `origin` it has pushed `main` to.
    fn operator_checkout(dir: &Path) -> std::path::PathBuf {
        let origin = dir.join("origin.git");
        let checkout = dir.join("checkout");
        std::fs::create_dir_all(&origin).unwrap();
        std::fs::create_dir_all(&checkout).unwrap();
        git_in(&origin, &["init", "-q", "--bare", "--initial-branch=main"]);
        for args in [
            vec!["init", "-q", "--initial-branch=main"],
            vec!["config", "user.email", "autopilot@example.test"],
            vec!["config", "user.name", "Autopilot Test"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "core.hooksPath", "/dev/null"],
        ] {
            git_in(&checkout, &args);
        }
        commit_in(&checkout, "seed", &[]);
        git_in(
            &checkout,
            &["remote", "add", "origin", origin.to_str().unwrap()],
        );
        git_in(&checkout, &["push", "-q", "-u", "origin", "main"]);
        checkout
    }

    /// The issue's real worktree, cut from the operator checkout's `HEAD`.
    fn remote_fixture() -> (tempfile::TempDir, Worktree) {
        let dir = tempfile::tempdir().unwrap();
        let checkout = operator_checkout(dir.path());
        let wt = worktree::ensure_worktree(&checkout, &dir.path().join("trees"), &issue(), "HEAD")
            .unwrap();
        (dir, wt)
    }

    /// Someone else lands an unsigned commit on `origin/main`; returns it.
    fn push_upstream_commit(dir: &Path) -> String {
        let upstream = dir.join("upstream");
        let origin = dir.join("origin.git");
        git_in(
            dir,
            &[
                "clone",
                "-q",
                origin.to_str().unwrap(),
                upstream.to_str().unwrap(),
            ],
        );
        for args in [
            vec!["config", "user.email", "upstream@example.test"],
            vec!["config", "user.name", "Upstream"],
            vec!["config", "commit.gpgsign", "false"],
            vec!["config", "core.hooksPath", "/dev/null"],
        ] {
            git_in(&upstream, &args);
        }
        let sha = commit_in(&upstream, "upstream", &[]);
        git_in(&upstream, &["push", "-q", "origin", "main"]);
        sha
    }

    #[test]
    fn upstream_commits_merged_in_from_the_remote_are_not_blamed() {
        let (dir, wt) = remote_fixture();
        let agent1 = commit_in(&wt.path, "agent1", &[]);
        git_in(&wt.path, &["push", "-q", "origin", &wt.branch]);
        let upstream = push_upstream_commit(dir.path());
        git_in(&wt.path, &["fetch", "-q", "origin"]);
        git_in(&wt.path, &["merge", "-q", "--no-edit", "origin/main"]);
        let merge = git_in(&wt.path, &["rev-parse", "HEAD"]);
        let agent2 = commit_in(&wt.path, "agent2", &[]);
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let violation = check(&wt).unwrap().unwrap();
        assert_eq!(
            blamed(&violation),
            [short(&agent1), short(&merge), short(&agent2)].into(),
            "only the agent's commits, never upstream {}: {violation}",
            short(&upstream)
        );
    }

    #[test]
    fn upstream_commits_rebased_onto_from_the_remote_are_not_blamed() {
        let (dir, wt) = remote_fixture();
        let agent1 = commit_in(&wt.path, "agent1", &[]);
        git_in(&wt.path, &["push", "-q", "origin", &wt.branch]);
        let upstream = push_upstream_commit(dir.path());
        git_in(&wt.path, &["fetch", "-q", "origin"]);
        git_in(&wt.path, &["rebase", "-q", "origin/main"]);
        let rebased = git_in(&wt.path, &["rev-parse", "HEAD"]);
        let agent2 = commit_in(&wt.path, "agent2", &[]);
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        // Until the rebase is pushed, the pre-rebase commit still ships.
        let violation = check(&wt).unwrap().unwrap();
        assert_eq!(
            blamed(&violation),
            [short(&agent1), short(&rebased), short(&agent2)].into(),
            "{violation}"
        );
        git_in(&wt.path, &["push", "-q", "--force", "origin", &wt.branch]);
        let violation = check(&wt).unwrap().unwrap();
        assert_eq!(
            blamed(&violation),
            [short(&rebased), short(&agent2)].into(),
            "only the agent's commits, never upstream {}: {violation}",
            short(&upstream)
        );
    }

    #[test]
    fn a_pushed_unsigned_commit_from_an_earlier_dispatch_still_fails_a_later_met() {
        let db = approved_db();
        let (_dir, wt) = remote_fixture();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let branch = wt.branch.clone();
        let mut calls = 0;
        let mut first = String::new();
        let mut dispatcher = CommitThenMet(|path: &Path| {
            calls += 1;
            if calls == 1 {
                first = commit_in(path, "work", &["-c", "commit.gpgsign=false"]);
                git_in(path, &["push", "-q", "origin", &branch]);
            }
        });
        let mut c = config();
        c.attempt_cap = 2;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut dispatcher).unwrap();
        assert_eq!(run.dispatches.len(), 2);
        for d in &run.dispatches {
            assert_eq!(d.classification, DispatchClassification::FailedAttempt);
            assert!(d.reason.as_deref().unwrap().contains(&first[..12]));
        }
    }

    #[test]
    fn a_policy_turned_off_in_an_earlier_run_still_fails_a_later_met() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        git_in(&wt.path, &["config", "commit.gpgsign", "true"]);
        let mut first = CommitThenMet(|path: &Path| {
            git_in(path, &["config", "commit.gpgsign", "false"]);
        });
        let mut c = config();
        c.attempt_cap = 1;
        run_issue(&db, &issue(), &brief(), &wt, &c, &mut first).unwrap();
        // A fresh run reads the policy as off; the branch remembers it was on.
        let mut second = CommitThenMet(|path: &Path| {
            commit_in(path, "work", &[]);
        });
        c.attempt_cap = 2;
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut second).unwrap();
        assert_eq!(run.dispatches.len(), 1);
        assert_eq!(
            run.dispatches[0].classification,
            DispatchClassification::FailedAttempt
        );
        let reason = run.dispatches[0].reason.as_deref().unwrap();
        assert!(reason.contains("was turned off"), "got: {reason}");
    }

    #[test]
    fn a_policy_that_was_never_on_is_not_enforced_and_leaves_no_flags() {
        let db = approved_db();
        let (_dir, wt) = remote_fixture(); // commit.gpgsign pinned false
        let mut dispatcher = CommitThenMet(|path: &Path| {
            commit_in(path, "work", &[]);
        });
        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        let branch_flag = format!("branch.{}.ironmemSigning", wt.branch);
        for key in [branch_flag.as_str(), "ironmem.autopilotSigning"] {
            let mut c = std::process::Command::new("git");
            crate::mcp::tools::scrub_git_environment(&mut c);
            let out = c
                .args(["config", "--get", key])
                .current_dir(&wt.path)
                .output()
                .unwrap();
            assert_eq!(out.status.code(), Some(1), "no {key} may be recorded");
        }
    }

    // ── config validation ───────────────────────────────────────────────

    #[test]
    fn default_config_is_valid_and_clears_the_turn_headroom_floor() {
        let config = config();
        config.validate().unwrap();
        assert!(config.max_turns >= config.n_turns * MIN_TOOL_TURNS_PER_GOAL_TURN);
    }

    #[test]
    fn config_rejects_max_turns_without_headroom_over_n() {
        // Rung 2's exact measured pairing: N=1 with --max-turns 4 needed 5
        // turns and returned no verdict at all.
        let mut config = config();
        config.n_turns = 1;
        config.max_turns = 4;
        let err = config.validate().unwrap_err();
        assert!(err.to_string().contains("max_turns"), "unexpected: {err}");
    }

    #[test]
    fn the_turn_ceiling_scales_with_n_rather_than_sitting_a_few_turns_above_it() {
        // The first live run's configuration (2026-09-08, issue #285):
        // `--n-turns 6` under the old additive rule derived `--max-turns 12`,
        // and every dispatch was killed mid-edit, because one goal turn
        // spends far more than two tool-call turns. The pairing is now
        // refused where it is made, rather than diagnosed afterwards from
        // three dead dispatches.
        let mut broken = config();
        broken.n_turns = 6;
        broken.max_turns = 12;
        let err = broken.validate().unwrap_err().to_string();
        assert!(err.contains("must be at least 60"), "unexpected: {err}");
        assert!(
            err.contains("count different things"),
            "the message must name the unit confusion, not just the number: {err}"
        );

        // The floor itself is permitted, and the default clears it with room
        // to spare — `--max-turns` is a runaway backstop, and budget and
        // wall-clock are what should bind first.
        let mut at_floor = config();
        at_floor.n_turns = 6;
        at_floor.max_turns = 60;
        assert!(at_floor.validate().is_ok());
        assert!(config().max_turns > 6 * MIN_TOOL_TURNS_PER_GOAL_TURN);
    }

    #[test]
    fn config_rejects_a_per_dispatch_ceiling_above_the_daily_cap() {
        let mut config = config();
        config.max_budget_usd = 50.0;
        config.daily_budget_usd = 10.0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("max_budget_usd"), "{err}");
    }

    #[test]
    fn a_codex_daily_cap_below_its_reservation_names_the_flag_it_can_set() {
        let mut config = codex_config();
        config.daily_budget_usd = 2.0;
        let err = config.validate().unwrap_err().to_string();
        assert!(err.contains("--daily-budget-usd to at least 2.5"), "{err}");
        assert!(!err.contains("max_budget_usd"), "{err}");
        config.daily_budget_usd = 2.5;
        config.validate().unwrap();
    }

    #[test]
    fn config_rejects_zero_valued_caps() {
        for mutate in [
            (|c: &mut RunConfig| c.n_turns = 0) as fn(&mut RunConfig),
            |c: &mut RunConfig| c.attempt_cap = 0,
            |c: &mut RunConfig| c.max_consecutive_infrastructure_failures = 0,
        ] {
            let mut config = config();
            mutate(&mut config);
            assert!(config.validate().is_err());
        }
    }

    // ── classification ──────────────────────────────────────────────────

    #[test]
    fn classification_follows_the_spec_error_table() {
        assert_eq!(classify(&met()), DispatchClassification::Met);
        assert_eq!(classify(&not_met()), DispatchClassification::FailedAttempt);
        assert_eq!(classify(&impossible()), DispatchClassification::Impossible);
        assert_eq!(
            classify(&no_verdict_error()),
            DispatchClassification::InfrastructureFailure
        );
    }

    #[test]
    fn a_completed_dispatch_with_no_verdict_is_an_attempt_not_infrastructure() {
        // The spec's anti-stall row: "Returns control with the goal still
        // set ... it counts as an attempt." Only an *errored* no-verdict
        // dispatch is infrastructure.
        let anti_stall = outcome(None, false, true, 0.10);
        assert_eq!(classify(&anti_stall), DispatchClassification::FailedAttempt);
    }

    #[test]
    fn a_dispatch_that_did_nothing_at_all_is_infrastructure_not_an_attempt() {
        // The anti-stall row's floor. "Returns control with the goal still
        // set" describes a dispatch that took the goal up and got nowhere;
        // this one never started, and has nothing to teach the next one.
        assert_eq!(
            classify(&resume_noop()),
            DispatchClassification::InfrastructureFailure
        );
        assert!(
            !classify(&resume_noop()).consumes_attempt(),
            "a dispatch that ran no turns and spent nothing must not burn an attempt"
        );
    }

    #[test]
    fn the_floor_needs_both_readings_and_neither_alone() {
        // Billed but turnless: it engaged the model, so it is a real, if
        // short, attempt.
        let billed = DispatchOutcome {
            cost_usd: Some(0.03),
            ..resume_noop()
        };
        assert_eq!(classify(&billed), DispatchClassification::FailedAttempt);

        // Turns but no recorded spend: `0.0` is not proof of doing nothing —
        // it is exactly what an unpriced dispatch reports.
        let unpriced = DispatchOutcome {
            num_turns: 4,
            ..resume_noop()
        };
        assert_eq!(classify(&unpriced), DispatchClassification::FailedAttempt);
    }

    #[test]
    fn a_killed_dispatch_is_still_read_as_a_timeout_not_as_an_empty_one() {
        // Ordering: a timeout reports no meter and zero turns. Its arm
        // sits above the floor so the diagnostic
        // stays a timeout, and the wall-clock ceiling keeps counting it.
        assert!(timed_out().num_turns == 0 && timed_out().cost_usd.is_none());
        assert_eq!(
            classify(&timed_out()),
            DispatchClassification::InfrastructureFailure
        );
    }

    #[test]
    fn repeated_noop_resumes_stop_the_run_instead_of_eating_the_attempt_cap() {
        // The first live run's actual failure (2026-09-08, issue #285): one
        // real dispatch returned `not_met`, then four resumes no-opped in 25
        // seconds and were each read as a failed attempt, driving the issue
        // to `AttemptCapExhausted` with four of its five attempts spent on
        // dispatches that never ran. The run must now stop on the
        // consecutive-infrastructure bound instead, leaving the attempt
        // budget for real work.
        let db = approved_db();
        let (_dir, worktree) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![
            Ok(not_met()),
            Ok(resume_noop()),
            Ok(resume_noop()),
            Ok(resume_noop()),
        ]);
        let run = run_issue(
            &db,
            &issue(),
            &brief(),
            &worktree,
            &config(),
            &mut dispatcher,
        )
        .unwrap();

        assert!(
            matches!(
                run.terminal,
                TerminalReason::InfrastructureFailure { consecutive: 3 }
            ),
            "expected the infrastructure bound, got {:?}",
            run.terminal
        );
        assert_eq!(
            run.dispatches.len(),
            4,
            "the run must stop on the third empty dispatch, not keep launching"
        );
        assert_eq!(
            run.cumulative_attempt_n, 1,
            "only the dispatch that actually ran may count against the cap"
        );
    }

    #[test]
    fn a_dead_process_is_never_believed_even_when_it_claimed_a_verdict() {
        for verdict in [Verdict::Met, Verdict::Impossible, Verdict::NotMet] {
            let died = outcome(Some(verdict), false, false, 0.10);
            assert_eq!(
                classify(&died),
                DispatchClassification::InfrastructureFailure,
                "a non-zero exit must not consume an attempt on {verdict:?}"
            );
        }
    }

    #[test]
    fn only_infrastructure_failures_are_exempt_from_the_attempt_cap() {
        assert!(DispatchClassification::Met.consumes_attempt());
        assert!(DispatchClassification::Impossible.consumes_attempt());
        assert!(DispatchClassification::FailedAttempt.consumes_attempt());
        assert!(!DispatchClassification::InfrastructureFailure.consumes_attempt());
    }

    // ── gate-config gating (spec's Testing table) ───────────────────────

    #[test]
    fn work_is_refused_on_a_pending_unapproved_repo_config() {
        let db = Database::open_in_memory().unwrap();
        gate_config::propose_gate_config(
            &db,
            &issue().repo,
            vec!["cargo test".to_string()],
            Vec::new(),
        )
        .unwrap();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);

        let err = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap_err();

        assert!(
            err.to_string().contains("not approved"),
            "unexpected: {err}"
        );
        assert!(
            dispatcher.seen.is_empty(),
            "an unapproved repo must never reach a dispatch"
        );
    }

    #[test]
    fn work_is_refused_when_no_gate_config_exists_at_all() {
        let db = Database::open_in_memory().unwrap();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        let err = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap_err();
        assert!(err.to_string().contains("onboard"), "unexpected: {err}");
        assert!(dispatcher.seen.is_empty());
    }

    // ── the happy path, end to end ──────────────────────────────────────

    #[test]
    fn a_met_dispatch_records_success_banks_cost_and_clears_dispatch_state() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        assert_eq!(run.cumulative_attempt_n, 1);
        assert_eq!(run.dispatches.len(), 1);

        // Lineage: one success attempt, carrying the worktree's real HEAD.
        let attempts = lineage::attempts_for_issue(&db, &issue()).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].verdict, AttemptOutcome::Success);
        assert!(
            attempts[0]
                .commit_sha
                .as_deref()
                .is_some_and(|s| s.len() >= 7),
            "a success must record the branch's commit sha"
        );

        // Budget: the dispatch's cost is on today's ledger.
        let ledger = budget::get_daily_spend(&db, &today_utc()).unwrap().unwrap();
        assert!((ledger.total_cost_usd - 0.20).abs() < 1e-9);
        assert_eq!(ledger.dispatch_count, 1);

        // Dispatch state: cleared, because the issue is finished.
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
    }

    #[test]
    fn the_first_dispatch_opens_a_session_and_a_concluded_one_is_replaced() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(met())]);

        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert_eq!(dispatcher.seen.len(), 2);
        let uuid = match &dispatcher.seen[0].session {
            SessionMode::New { session_uuid } => session_uuid.clone(),
            other => panic!("first dispatch must open a new session, got {other:?}"),
        };
        // The first dispatch here returned a verdict, so its goal loop
        // concluded and its session is spent — see the rotation in
        // `run_issue`. It is replaced rather than resumed.
        match &dispatcher.seen[1].session {
            SessionMode::New { session_uuid } => assert_ne!(
                session_uuid, &uuid,
                "the replacement session must not reuse the spent uuid"
            ),
            other => panic!("a concluded goal loop must be replaced, got {other:?}"),
        }
    }

    #[test]
    fn an_interrupted_dispatch_is_resumed_rather_than_replaced() {
        // The majority case, and the one `--resume` was chosen for: a
        // dispatch that returned **no** verdict was cut off mid-goal — killed
        // on the wall-clock bound, stopped by `--max-turns`, or crashed — so
        // its Stop hook is still armed and resuming it is both correct and
        // cheap. Only a concluded goal loop is replaced.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(timed_out()), Ok(met())]);

        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert_eq!(dispatcher.seen.len(), 2);
        let uuid = match &dispatcher.seen[0].session {
            SessionMode::New { session_uuid } => session_uuid.clone(),
            other => panic!("first dispatch must open a new session, got {other:?}"),
        };
        match &dispatcher.seen[1].session {
            SessionMode::Resume { session_uuid } => assert_eq!(session_uuid, &uuid),
            other => panic!("an interrupted dispatch must be resumed, got {other:?}"),
        }
    }

    #[test]
    fn the_replacement_session_is_recorded_before_it_is_used() {
        // Crash safety: if the Lead dies between a `not_met` and the next
        // dispatch, whatever it wrote must send its successor to the *new*
        // session. Recording the spent uuid — or the new one marked claimed —
        // would send the successor straight back into the session that cannot
        // respond, which is the failure this whole rotation exists to end.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        // Stop on the first launch failure so the run pauses with its state
        // intact instead of clearing it on a terminal verdict.
        config.max_consecutive_infrastructure_failures = 1;
        let mut dispatcher = ScriptedDispatcher::new(vec![
            Ok(not_met()),
            Err(MemoryError::NotFound("launch failed".into())),
        ]);

        run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        let spent = match &dispatcher.seen[0].session {
            SessionMode::New { session_uuid } => session_uuid.clone(),
            other => panic!("first dispatch must open a new session, got {other:?}"),
        };
        let replacement = match &dispatcher.seen[1].session {
            SessionMode::New { session_uuid } => session_uuid.clone(),
            other => panic!("a concluded goal loop must be replaced, got {other:?}"),
        };
        assert_ne!(spent, replacement);

        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .expect("a paused run keeps its dispatch state");
        assert_eq!(
            state.session_uuid, replacement,
            "the drawer must name the session the next dispatch will use"
        );
        assert!(
            !state.session_claimed,
            "a replacement session no process has opened yet must not be recorded as claimed, \
             or a resuming Lead would --resume a uuid that has never existed"
        );
    }

    #[test]
    fn a_concluded_goal_loops_replacement_carries_across_runs() {
        // The crash-recovery half of the rotation. A later run adopts the
        // replacement uuid from the drawer rather than minting another, and
        // opens it with `--session-id` — because no process has ever claimed
        // it. Resuming it instead would `--resume` a session that does not
        // exist; minting a third would leave the recorded one orphaned.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut paused = config();
        paused.daily_budget_usd = 2.6;
        paused.max_budget_usd = 2.5;
        let mut first = ScriptedDispatcher::new(vec![Ok(not_met())]);
        run_issue(&db, &issue(), &brief(), &wt, &paused, &mut first).unwrap();

        let stored = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .expect("a budget pause must keep the dispatch state");
        assert!(!stored.session_claimed);

        let mut second = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut second).unwrap();
        match &second.seen[0].session {
            SessionMode::New { session_uuid } => {
                assert_eq!(
                    session_uuid, &stored.session_uuid,
                    "the next run must adopt the recorded replacement, not mint another"
                );
            }
            other => panic!("an unclaimed session must be opened, not resumed, got {other:?}"),
        }
    }

    #[test]
    fn a_resumed_run_reuses_the_stored_session_uuid() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        // First run stops on the daily budget, leaving the state in place.
        // The dispatch returns no verdict on purpose: it was interrupted
        // rather than concluded, so its session is still resumable. A
        // `not_met` here would rotate the session instead — see
        // `a_concluded_goal_loops_replacement_carries_across_runs`.
        let mut paused = config();
        paused.daily_budget_usd = 2.6;
        paused.max_budget_usd = 2.5;
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(no_verdict_error())]);
        run_issue(&db, &issue(), &brief(), &wt, &paused, &mut dispatcher).unwrap();
        let stored = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .expect("a budget pause must keep the dispatch state");

        // The second run adopts it rather than opening a fresh session.
        let mut second = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut second).unwrap();
        match &second.seen[0].session {
            SessionMode::Resume { session_uuid } => {
                assert_eq!(session_uuid, &stored.session_uuid);
            }
            other => panic!("a resumed run must not open a new session, got {other:?}"),
        }
    }

    #[test]
    fn prior_attempts_reach_the_next_dispatchs_turn_prompt() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(met())]);

        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert!(
            dispatcher.seen[0].condition.contains("none yet"),
            "the first dispatch has no lineage to show"
        );
        assert!(
            dispatcher.seen[1].condition.contains("scripted NotMet"),
            "the second dispatch must see the first attempt's record:\n{}",
            dispatcher.seen[1].condition
        );
    }

    #[test]
    fn the_goal_condition_is_generated_from_the_approved_gate_config() {
        // The spec's "Goal condition and approved gates disagree — cannot
        // occur by construction" row, guarded.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();
        assert!(dispatcher.seen[0]
            .condition
            .contains("cargo test --workspace"));
    }

    #[test]
    fn a_run_on_an_already_succeeded_issue_dispatches_nothing() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut first = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut first).unwrap();

        let mut second = ScriptedDispatcher::new(vec![]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut second).unwrap();

        assert!(matches!(
            run.terminal,
            TerminalReason::AlreadySucceeded { .. }
        ));
        assert!(second.seen.is_empty(), "settled work must not be re-run");
        assert_eq!(run.total_cost_usd, 0.0);
    }

    #[test]
    fn a_remediation_re_opens_a_succeeded_issue_and_tells_the_ic_what_to_fix() {
        // Rung 11's whole point. The guard that makes an issue finished is
        // the guard in the way, and the narrow exception is a reviewer's
        // recorded objection — delivered into the condition, not merely
        // recorded.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut first = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut first).unwrap();

        remediate::arm_remediation(
            &db,
            &remediate::ArmRequest {
                issue: &issue(),
                pr_number: 42,
                head_sha: "deadbeef",
                findings: Some("the retry loop is unbounded"),
                attempt_cap: config().attempt_cap,
            },
        )
        .unwrap();

        let mut second = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut second).unwrap();

        assert!(
            matches!(run.terminal, TerminalReason::Met { .. }),
            "a remediation dispatch runs; got {:?}",
            run.terminal
        );
        assert_eq!(second.seen.len(), 1, "exactly one remediation dispatch");
        let condition = &second.seen[0].condition;
        assert!(condition.contains("the retry loop is unbounded"));
        assert!(condition.contains("pull request #42"));
        assert!(
            condition.contains("addressed by one of those commits"),
            "the findings must reach the CONDITION, or the already-green gate \
satisfies it on its own"
        );
        // The worktree's real branch, not a re-derivation, reaches the
        // condition — `run_issue` is the only place that holds both the
        // `Worktree` and the prompt.
        assert!(
            condition.contains(&wt.branch),
            "the condition must name the branch the dispatch is actually on"
        );
    }

    #[test]
    fn a_succeeded_issue_with_no_remediation_is_still_left_alone() {
        // Rung 9's lesson 43: the new switch must not change the behaviour of
        // everything that predates it.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut first = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut first).unwrap();

        let mut second = ScriptedDispatcher::new(vec![]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut second).unwrap();
        assert!(matches!(
            run.terminal,
            TerminalReason::AlreadySucceeded { .. }
        ));
    }

    #[test]
    fn a_success_on_the_last_attempt_can_still_be_remediated() {
        // Issue #344's boundary. The original work succeeds on attempt N of
        // N, a reviewer asks for changes, and the remediation must still
        // dispatch: its budget is its own, counted from the first success,
        // not whatever the success happened to leave over. Before the fix
        // `arm_remediation` answered `CapReached` and armed nothing.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.attempt_cap = 2;

        let mut first = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(met())]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut first).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        assert_eq!(run.cumulative_attempt_n, 2, "setup: success at 2 of 2");

        let outcome = remediate::arm_remediation(
            &db,
            &remediate::ArmRequest {
                issue: &issue(),
                pr_number: 42,
                head_sha: "deadbeef",
                findings: Some("fix it"),
                attempt_cap: 2,
            },
        )
        .unwrap();
        assert!(outcome.in_force(), "got {outcome:?}");

        let mut second = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut second).unwrap();
        assert_eq!(second.seen.len(), 1, "the remediation dispatch ran");
        assert!(
            matches!(run.terminal, TerminalReason::Met { .. }),
            "got {:?}",
            run.terminal
        );
        // The lifetime counter keeps numbering, so the prompt's history of
        // attempts stays unambiguous.
        assert_eq!(run.cumulative_attempt_n, 3);
    }

    #[test]
    fn a_remediation_gets_a_full_cap_of_its_own_and_then_stops() {
        // The budget is independent of the original work, and still a bound:
        // the remediation spends exactly `attempt_cap` dispatches and the run
        // then stops on the cap rather than re-dispatching for ever.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.attempt_cap = 2;

        let mut first = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config, &mut first).unwrap();

        remediate::arm_remediation(
            &db,
            &remediate::ArmRequest {
                issue: &issue(),
                pr_number: 42,
                head_sha: "deadbeef",
                findings: Some("fix it"),
                attempt_cap: 2,
            },
        )
        .unwrap();

        let mut second = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(not_met())]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut second).unwrap();
        assert_eq!(
            second.seen.len(),
            2,
            "a full cap of remediation dispatches, not the one the success left"
        );
        assert!(
            matches!(run.terminal, TerminalReason::AttemptCapExhausted),
            "got {:?}",
            run.terminal
        );
        assert_eq!(run.cumulative_attempt_n, 3);

        // And it stays stopped: the next run does not dispatch again.
        let mut third = ScriptedDispatcher::new(vec![]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut third).unwrap();
        assert_eq!(run.terminal, TerminalReason::AttemptCapExhausted);
        assert!(third.seen.is_empty());
    }

    #[test]
    fn a_remediation_re_armed_at_a_new_head_keeps_spending_the_same_budget() {
        // A remediation dispatch that pushes a commit and fails still moves
        // the PR head, and a second `needs_changes` about that head arms a
        // new record. It is a new round of the same remediation, so the run
        // that follows gets what the first round left — not another cap.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.attempt_cap = 3;

        let mut first = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config, &mut first).unwrap();

        let arm = |head: &str| {
            remediate::arm_remediation(
                &db,
                &remediate::ArmRequest {
                    issue: &issue(),
                    pr_number: 42,
                    head_sha: head,
                    findings: Some("fix it"),
                    attempt_cap: 3,
                },
            )
            .unwrap()
        };
        assert!(arm("deadbeef").in_force());

        // Round one: two failed dispatches, then the daily budget pauses the
        // run (each scripted dispatch banks $0.20; the next would need $2.50
        // of headroom with $0.60 already spent).
        let mut paused = config.clone();
        paused.daily_budget_usd = 3.0;
        paused.max_budget_usd = 2.5;
        let mut second = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(not_met())]);
        run_issue(&db, &issue(), &brief(), &wt, &paused, &mut second).unwrap();
        assert_eq!(second.seen.len(), 2, "setup: two of three spent");

        // Round two, about the commit round one pushed.
        assert!(arm("feedface").in_force());
        let mut third = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(not_met())]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut third).unwrap();
        assert_eq!(
            third.seen.len(),
            1,
            "only the one attempt round one left, not a fresh cap"
        );
        assert_eq!(run.terminal, TerminalReason::AttemptCapExhausted);
        assert!(
            !arm("cafef00d").in_force(),
            "and a third head is refused: the remediation is exhausted"
        );
    }

    // ── attempt cap ─────────────────────────────────────────────────────

    #[test]
    fn failed_attempts_stop_at_the_cap_and_write_a_terminal_record() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.attempt_cap = 3;
        let mut dispatcher =
            ScriptedDispatcher::new(vec![Ok(not_met()), Ok(not_met()), Ok(not_met())]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        assert_eq!(run.terminal, TerminalReason::AttemptCapExhausted);
        assert_eq!(dispatcher.seen.len(), 3, "the cap bounds the dispatches");
        assert_eq!(run.cumulative_attempt_n, 3);

        let attempts = lineage::attempts_for_issue(&db, &issue()).unwrap();
        assert_eq!(attempts.len(), 4, "3 attempts plus the terminal record");
        assert!(
            attempts
                .iter()
                .any(|a| a.approach.contains("terminal: per-issue attempt cap")),
            "the spec requires a terminal lineage record at the cap"
        );
        // Terminal, so the issue does not self-resume.
        assert!(dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .is_none());
    }

    #[test]
    fn the_attempt_cap_is_cumulative_across_runs_not_per_run() {
        // The spec's cross-dispatch stagnation control: the counter persists
        // in the issue-status drawer, so a fresh run cannot re-earn attempts
        // by starting over.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.attempt_cap = 2;

        let mut first = ScriptedDispatcher::new(vec![Ok(not_met())]);
        // Pause the first run on budget after exactly one attempt.
        let mut paused = config.clone();
        paused.daily_budget_usd = 2.6;
        paused.max_budget_usd = 2.5;
        run_issue(&db, &issue(), &brief(), &wt, &paused, &mut first).unwrap();
        assert_eq!(first.seen.len(), 1);

        let mut second = ScriptedDispatcher::new(vec![Ok(not_met())]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut second).unwrap();

        assert_eq!(second.seen.len(), 1, "only the remaining attempt is left");
        assert_eq!(run.terminal, TerminalReason::AttemptCapExhausted);
        assert_eq!(run.cumulative_attempt_n, 2);
    }

    #[test]
    fn an_issue_already_at_its_cap_stops_without_dispatching() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        lineage::upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Failed),
                best_commit_sha: None,
                cumulative_attempt_n: 5,
            },
        )
        .unwrap();
        let mut dispatcher = ScriptedDispatcher::new(vec![]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert_eq!(run.terminal, TerminalReason::AttemptCapExhausted);
        assert!(
            dispatcher.seen.is_empty(),
            "`agent:exhausted` never self-resumes"
        );
    }

    #[test]
    fn a_human_retry_gives_a_capped_issue_its_budget_back() {
        // The recovery the spec names — "only a human re-labeling it
        // retries" — which never worked, because the label governs selection
        // and the cap is enforced against a counter no label touches. The
        // grant is what makes re-labeling mean something.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        lineage::upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Failed),
                best_commit_sha: None,
                cumulative_attempt_n: 5,
            },
        )
        .unwrap();
        retry::grant_retry(&db, &issue(), 5).unwrap();

        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(met())]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert!(
            matches!(run.terminal, TerminalReason::Met { .. }),
            "a forgiven issue must dispatch, got {:?}",
            run.terminal
        );
        assert_eq!(dispatcher.seen.len(), 1);
        // The lifetime counter keeps counting, so the next attempt is #6 and
        // the prompt's history stays unambiguous.
        assert_eq!(run.cumulative_attempt_n, 6);
    }

    #[test]
    fn a_retry_grants_one_cap_not_an_unlimited_one() {
        // Forgiveness is a one-time credit, not a raised ceiling: the issue
        // gets `attempt_cap` more attempts and then stops again, which is the
        // whole point of the cap surviving the recovery.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        lineage::upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Failed),
                best_commit_sha: None,
                cumulative_attempt_n: 5,
            },
        )
        .unwrap();
        retry::grant_retry(&db, &issue(), 5).unwrap();

        let mut config = config();
        config.attempt_cap = 2;
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(not_met())]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        assert_eq!(run.terminal, TerminalReason::AttemptCapExhausted);
        assert_eq!(
            dispatcher.seen.len(),
            2,
            "exactly the forgiven cap's worth of attempts, then stop again"
        );
        assert_eq!(run.cumulative_attempt_n, 7);
    }

    // ── impossible ──────────────────────────────────────────────────────

    #[test]
    fn an_impossible_verdict_records_a_failed_attempt_and_stops() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(impossible())]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert!(matches!(run.terminal, TerminalReason::Impossible { .. }));
        assert_eq!(
            dispatcher.seen.len(),
            1,
            "an impossible goal is not retried"
        );
        let attempts = lineage::attempts_for_issue(&db, &issue()).unwrap();
        assert_eq!(attempts.len(), 1);
        assert_eq!(attempts[0].verdict, AttemptOutcome::Failed);
        assert!(attempts[0]
            .why_failed
            .as_deref()
            .unwrap()
            .contains("impossible"));
    }

    // ── infrastructure failures ─────────────────────────────────────────

    #[test]
    fn infrastructure_failures_bank_cost_but_never_consume_an_attempt() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(no_verdict_error()), Ok(met())]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        assert_eq!(
            run.cumulative_attempt_n, 1,
            "only the met dispatch consumed an attempt"
        );
        assert_eq!(
            lineage::attempts_for_issue(&db, &issue()).unwrap().len(),
            1,
            "an infrastructure failure writes no lineage attempt"
        );
        // Both dispatches were still billed.
        let ledger = budget::get_daily_spend(&db, &today_utc()).unwrap().unwrap();
        assert_eq!(ledger.dispatch_count, 2);
        assert!((ledger.total_cost_usd - 0.40).abs() < 1e-9);
    }

    #[test]
    fn consecutive_infrastructure_failures_are_bounded() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.max_consecutive_infrastructure_failures = 2;
        let mut dispatcher = ScriptedDispatcher::new(vec![
            Ok(no_verdict_error()),
            Ok(no_verdict_error()),
            Ok(met()),
        ]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        assert_eq!(
            run.terminal,
            TerminalReason::InfrastructureFailure { consecutive: 2 }
        );
        assert_eq!(dispatcher.seen.len(), 2, "the bound must stop the loop");
        // Non-terminal for the issue: the session stays resumable.
        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .expect("an infrastructure pause must keep the dispatch state");
        assert_eq!(state.state, "paused-infrastructure-failure");
    }

    #[test]
    fn a_real_attempt_resets_the_consecutive_infrastructure_counter() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.max_consecutive_infrastructure_failures = 2;
        config.attempt_cap = 2;
        let mut dispatcher = ScriptedDispatcher::new(vec![
            Ok(no_verdict_error()),
            Ok(not_met()),
            Ok(no_verdict_error()),
            Ok(met()),
        ]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        assert!(
            matches!(run.terminal, TerminalReason::Met { .. }),
            "a transient failure between real attempts must not exhaust the bound: {:?}",
            run.terminal
        );
        assert_eq!(dispatcher.seen.len(), 4);
    }

    #[test]
    fn a_dispatch_that_ran_but_failed_to_parse_leaves_the_session_claimed() {
        // The process ran, so its session uuid may already exist — a retry
        // must resume rather than re-claim it with `--session-id`.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![
            Err(MemoryError::Validation(
                "IC dispatch produced no parseable result JSON".into(),
            )),
            Ok(met()),
        ]);

        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert!(
            matches!(dispatcher.seen[1].session, SessionMode::Resume { .. }),
            "a retry after a process that ran must resume, got {:?}",
            dispatcher.seen[1].session
        );
    }

    #[test]
    fn a_dispatch_that_never_launched_leaves_the_session_unclaimed() {
        // No process started, so no session exists — the retry must still
        // open one with `--session-id`.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![
            Err(MemoryError::NotFound(
                "failed to launch IC dispatch: no such file".into(),
            )),
            Ok(met()),
        ]);

        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert!(
            matches!(dispatcher.seen[1].session, SessionMode::New { .. }),
            "a retry after a failed launch must still open the session, got {:?}",
            dispatcher.seen[1].session
        );
    }

    #[test]
    fn a_dispatch_that_produced_no_result_is_an_infrastructure_failure() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.max_consecutive_infrastructure_failures = 1;
        let mut dispatcher = ScriptedDispatcher::new(vec![Err(MemoryError::Validation(
            "IC dispatch produced no parseable result JSON".into(),
        ))]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        assert_eq!(
            run.terminal,
            TerminalReason::InfrastructureFailure { consecutive: 1 }
        );
        assert_eq!(run.cumulative_attempt_n, 0);
        assert_eq!(run.total_cost_usd, 0.0);
        assert!(
            budget::get_daily_spend(&db, &today_utc())
                .unwrap()
                .is_none(),
            "a dispatch that returned no meter bills nothing"
        );
    }

    // ── budget ──────────────────────────────────────────────────────────

    #[test]
    fn a_dispatch_is_refused_when_its_ceiling_could_carry_the_day_past_the_cap() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.max_budget_usd = 2.0;
        config.daily_budget_usd = 3.0;
        // Pre-bank enough that one more dispatch's *ceiling* would exceed the
        // cap, even though the spend so far is comfortably under it.
        budget::accumulate_daily_spend(&db, &today_utc(), 1.5).unwrap();
        let mut dispatcher = ScriptedDispatcher::new(vec![]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        match run.terminal {
            TerminalReason::DailyBudgetExhausted { spent_usd } => {
                assert!((spent_usd - 1.5).abs() < 1e-9);
            }
            other => panic!("expected a budget stop, got {other:?}"),
        }
        assert!(
            dispatcher.seen.is_empty(),
            "pre-authorization must refuse before spending, not after"
        );
        // Paused, not finished: the issue stays resumable.
        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .expect("a budget pause must keep the dispatch state");
        assert_eq!(state.state, "paused-daily-budget");
    }

    #[test]
    fn every_dispatch_that_returned_a_result_is_banked_including_failures() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.attempt_cap = 2;
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(not_met())]);

        let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        assert_eq!(run.terminal, TerminalReason::AttemptCapExhausted);
        let ledger = budget::get_daily_spend(&db, &today_utc()).unwrap().unwrap();
        assert_eq!(ledger.dispatch_count, 2);
        assert!((ledger.total_cost_usd - 0.40).abs() < 1e-9);
        assert!((run.total_cost_usd - 0.40).abs() < 1e-9);
    }

    // ── dispatch state ──────────────────────────────────────────────────

    #[test]
    fn dispatch_state_is_written_before_the_first_dispatch() {
        // The spec's crash-safe-state requirement: a Lead that dies during
        // its very first dispatch must still leave a drawer behind, or the
        // live IC becomes an orphan its restart cannot adopt.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        struct AssertingDispatcher<'a> {
            db: &'a Database,
        }
        impl Dispatcher for AssertingDispatcher<'_> {
            fn dispatch(
                &mut self,
                _repo: &Path,
                spec: &DispatchSpec,
            ) -> Result<DispatchOutcome, MemoryError> {
                let state = dispatch_state::get_dispatch_state(self.db, &issue())
                    .unwrap()
                    .expect("dispatch state must exist before the IC is launched");
                assert_eq!(
                    state.session_uuid,
                    match &spec.session {
                        SessionMode::New { session_uuid }
                        | SessionMode::Resume { session_uuid } => {
                            session_uuid.clone()
                        }
                    }
                );
                assert_eq!(state.dispatch_class, "logic");
                assert_eq!(state.ic_session_name, "ic-ironrace-ironmem-283");
                Ok(met())
            }
        }
        let mut dispatcher = AssertingDispatcher { db: &db };
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();
    }

    #[test]
    fn dispatch_state_records_the_worktree_the_ic_runs_in() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.daily_budget_usd = 2.6;
        config.max_budget_usd = 2.5;
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(not_met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config, &mut dispatcher).unwrap();

        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .unwrap();
        assert_eq!(state.worktree_path, wt.path.to_string_lossy());
        assert_eq!(state.turn_n, 3, "turns accumulate across dispatches");
    }

    #[test]
    fn a_pause_before_any_dispatch_leaves_the_session_unclaimed() {
        // The crash-safe drawer is written before the first launch, so a run
        // that stops on the daily budget without dispatching anything has a
        // session uuid no `claude` process has ever seen. Resuming it would
        // fail on every later pass, wedging the issue permanently.
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut broke = config();
        broke.max_budget_usd = 2.5;
        broke.daily_budget_usd = 2.6;
        budget::accumulate_daily_spend(&db, &today_utc(), 1.0).unwrap();
        let mut none = ScriptedDispatcher::new(vec![]);
        let run = run_issue(&db, &issue(), &brief(), &wt, &broke, &mut none).unwrap();
        assert!(matches!(
            run.terminal,
            TerminalReason::DailyBudgetExhausted { .. }
        ));
        assert!(none.seen.is_empty());
        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .expect("a budget pause keeps the state");
        assert!(
            !state.session_claimed,
            "no process ran, so the session was never opened"
        );

        // Tomorrow's run must still *open* the session, not resume a uuid
        // that does not exist.
        let mut second = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut second).unwrap();
        match &second.seen[0].session {
            SessionMode::New { session_uuid } => {
                assert_eq!(
                    session_uuid, &state.session_uuid,
                    "same uuid, freshly opened"
                )
            }
            other => panic!("an unclaimed session must be opened, not resumed: {other:?}"),
        }
    }

    #[test]
    fn a_paused_run_that_did_dispatch_resumes_its_claimed_session() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut paused = config();
        paused.daily_budget_usd = 2.6;
        paused.max_budget_usd = 2.5;
        // No verdict: interrupted, not concluded, so the session it opened is
        // still the one to go back to.
        let mut first = ScriptedDispatcher::new(vec![Ok(no_verdict_error())]);
        run_issue(&db, &issue(), &brief(), &wt, &paused, &mut first).unwrap();
        let state = dispatch_state::get_dispatch_state(&db, &issue())
            .unwrap()
            .unwrap();
        assert!(
            state.session_claimed,
            "a dispatch ran, so the session exists"
        );
    }

    #[test]
    fn re_running_an_exhausted_issue_does_not_append_another_terminal_record() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut config = config();
        config.attempt_cap = 1;
        let mut first = ScriptedDispatcher::new(vec![Ok(not_met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config, &mut first).unwrap();
        let after_first = lineage::attempts_for_issue(&db, &issue()).unwrap().len();

        for _ in 0..3 {
            let mut again = ScriptedDispatcher::new(vec![]);
            let run = run_issue(&db, &issue(), &brief(), &wt, &config, &mut again).unwrap();
            assert_eq!(run.terminal, TerminalReason::AttemptCapExhausted);
        }

        assert_eq!(
            lineage::attempts_for_issue(&db, &issue()).unwrap().len(),
            after_first,
            "a polling Lead must not grow the lineage on every pass over a settled issue"
        );
    }

    #[test]
    fn a_failed_attempt_never_downgrades_a_recorded_success() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut first = ScriptedDispatcher::new(vec![Ok(met())]);
        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut first).unwrap();
        let succeeded = lineage::get_issue_status(&db, &issue()).unwrap().unwrap();

        // Force a follow-up failed attempt directly through the same helper
        // the loop uses, bypassing the already-succeeded short circuit.
        record_and_advance(
            &db,
            &AttemptRecord {
                issue: issue(),
                attempt_n: succeeded.cumulative_attempt_n + 1,
                approach: "a later fix dispatch".to_string(),
                verdict: AttemptOutcome::Failed,
                why_failed: Some("still red".to_string()),
                commit_sha: None,
            },
            Some(&succeeded),
            None,
        )
        .unwrap();

        let after = lineage::get_issue_status(&db, &issue()).unwrap().unwrap();
        assert_eq!(after.best_verdict, Some(AttemptOutcome::Success));
        assert_eq!(after.best_commit_sha, succeeded.best_commit_sha);
        assert_eq!(after.cumulative_attempt_n, 2);
    }
    #[test]
    fn completed_codex_pauses_reenter_queue_and_resume_native_identity_once() {
        use crate::autopilot::gh::IssueListing;
        use crate::autopilot::process_tree::ProcessSnapshot;
        use crate::autopilot::queue::{plan_queue, QueueConfig, RepoBacklog};
        use crate::autopilot::registry::{HarnessSnapshots, RegistrySnapshot};
        use crate::autopilot::supervise::{reconcile, ReconcileVerdict};
        struct PausingDispatch<'a> {
            db: &'a Database,
            outcome: Option<DispatchOutcome>,
            spend_after: bool,
            invocation: Option<String>,
        }
        impl Dispatcher for PausingDispatch<'_> {
            fn implementer(&self) -> Implementer {
                Implementer::Codex
            }
            fn dispatch(
                &mut self,
                _repo: &Path,
                spec: &DispatchSpec,
            ) -> Result<DispatchOutcome, MemoryError> {
                let state = dispatch_state::get_dispatch_state(self.db, &issue())?.unwrap();
                assert_eq!(state.state, "dispatching");
                assert_eq!(
                    state.invocation_artifacts, spec.invocation_artifacts,
                    "pending invocation is durable before launch"
                );
                self.invocation = Some(
                    spec.invocation_artifacts
                        .as_ref()
                        .unwrap()
                        .invocation_id
                        .clone(),
                );
                if self.spend_after {
                    budget::accumulate_daily_spend(self.db, &today_utc(), 1.5)?;
                }
                let mut outcome = self
                    .outcome
                    .take()
                    .expect("only one invocation before pause");
                outcome.cost_usd = None;
                Ok(outcome)
            }
        }
        for (first, daily, infrastructure) in [
            (not_met(), false, false),
            (timed_out(), false, false),
            (not_met(), true, false),
            (no_verdict_error(), false, true),
        ] {
            let db = approved_db();
            let (_dir, wt) = fixture_worktree();
            let mut cfg = codex_config();
            cfg.max_budget_usd = 1.0;
            cfg.daily_budget_usd = 2.0;
            cfg.max_unpriced_dispatches_per_day = 1;
            if infrastructure {
                cfg.max_consecutive_infrastructure_failures = 1;
            }
            let mut dispatcher = PausingDispatch {
                db: &db,
                outcome: Some(first),
                spend_after: daily,
                invocation: None,
            };
            let paused = run_issue(&db, &issue(), &brief(), &wt, &cfg, &mut dispatcher).unwrap();
            assert_eq!(paused.unpriced_dispatches, 1);
            if daily {
                assert!(matches!(
                    paused.terminal,
                    TerminalReason::DailyBudgetExhausted { .. }
                ));
            } else if infrastructure {
                assert!(matches!(
                    paused.terminal,
                    TerminalReason::InfrastructureFailure { .. }
                ));
            } else {
                assert!(matches!(
                    paused.terminal,
                    TerminalReason::UnpricedDispatchesExhausted { .. }
                ));
            }
            let state = dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .unwrap();
            assert_eq!(
                state.state,
                if daily {
                    "paused-daily-budget"
                } else {
                    "paused-infrastructure-failure"
                }
            );
            assert!(state.invocation_artifacts.is_none());
            let native = state.native_session_id.clone().unwrap();
            assert_eq!(native, "11111111-2222-3333-4444-555555555555");
            let ledger = budget::get_daily_spend(&db, &today_utc()).unwrap().unwrap();
            assert_eq!(ledger.unpriced_dispatch_count, 1);
            assert_eq!(
                ledger.reserved_unpriced_dispatch_ids,
                vec![dispatcher.invocation.unwrap()]
            );
            let snapshots = HarnessSnapshots {
                claude: RegistrySnapshot::Unavailable {
                    reason: "Claude unrelated".into(),
                },
                processes: ProcessSnapshot::Available(Vec::new()),
            };
            let backlog = [RepoBacklog {
                repo: issue().repo,
                issues: vec![IssueListing {
                    number: issue().number,
                    title: brief().title,
                    body: brief().body,
                    labels: vec!["agent:ready".into()],
                    updated_at: "2026-09-02T00:00:00Z".into(),
                }],
            }];
            let queue_cfg = QueueConfig {
                max_budget_usd: 1.0,
                daily_budget_usd: 4.0,
                ..QueueConfig::default()
            };
            let plan = plan_queue(&db, &backlog, &snapshots, &queue_cfg).unwrap();
            assert_eq!(
                plan.dispatch.len(),
                1,
                "a completed paused Codex invocation must be selectable: {:?}",
                plan.deferred
            );
            assert_eq!(plan.dispatch[0].issue, issue());
            assert!(matches!(
                reconcile(&db, &snapshots).unwrap()[0].verdict,
                ReconcileVerdict::RestartFromCheckpoint { .. }
            ));
            assert_eq!(
                budget::get_daily_spend(&db, &today_utc()).unwrap().unwrap(),
                ledger,
                "liveness planning does not reserve the old invocation again"
            );
            cfg.daily_budget_usd = 4.0;
            cfg.max_unpriced_dispatches_per_day = 2;
            let mut resumed = codex_script(vec![met()]);
            resumed.recovery = Some(InvocationRecovery::NoPriorInvocation);
            let result = run_issue(&db, &issue(), &brief(), &wt, &cfg, &mut resumed).unwrap();
            assert!(matches!(result.terminal, TerminalReason::Met { .. }));
            assert_eq!(result.unpriced_dispatches, 1);
            assert!(
                matches!(&resumed.inner.seen[0].session, SessionMode::Resume { session_uuid } if session_uuid == &native)
            );
            let after = budget::get_daily_spend(&db, &today_utc()).unwrap().unwrap();
            assert_eq!(after.unpriced_dispatch_count, 2);
            assert_eq!(after.reserved_unpriced_dispatch_ids.len(), 2);
            assert_eq!(
                after.reserved_unpriced_dispatch_ids[1],
                resumed.inner.seen[0]
                    .invocation_artifacts
                    .as_ref()
                    .unwrap()
                    .invocation_id
            );
            assert_eq!(
                after.reserved_unpriced_dispatch_ids[0],
                ledger.reserved_unpriced_dispatch_ids[0]
            );
            assert_ne!(
                after.reserved_unpriced_dispatch_ids[0],
                after.reserved_unpriced_dispatch_ids[1]
            );
            assert_eq!(after.total_cost_usd, ledger.total_cost_usd);
            assert!(dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .is_none());
        }
    }
}
