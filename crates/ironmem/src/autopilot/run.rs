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

use super::codex_dispatch::{self, CodexArtifacts, CodexRecovery};
use super::dispatch::{self, DispatchOutcome, DispatchSpec, SessionMode, Verdict};
use super::implementer::Implementer;
use super::worktree::Worktree;
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
        if self.model.trim().is_empty() {
            let harness = match self.implementer {
                Implementer::Claude => "Claude",
                Implementer::Codex => "Codex",
            };
            return Err(MemoryError::Validation(format!(
                "model must not be empty for the {harness} implementer"
            )));
        }
        if self.implementer == Implementer::Codex && self.model.starts_with("claude-") {
            return Err(MemoryError::Validation(format!(
                "model '{}' is incompatible with the Codex implementer; choose a Codex model",
                self.model
            )));
        }
        if self.implementer == Implementer::Claude && self.model.starts_with("gpt-") {
            return Err(MemoryError::Validation(format!(
                "model '{}' is incompatible with the Claude implementer; choose a Claude model",
                self.model
            )));
        }
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
            return Err(MemoryError::Validation(format!(
                "max_budget_usd ({}) exceeds daily_budget_usd ({}) — no dispatch could ever be \
                 authorized",
                self.max_budget_usd, self.daily_budget_usd
            )));
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
    if outcome.verdict.is_none() && outcome.num_turns == 0 && outcome.cost_usd == Some(0.0) {
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
    /// Effective per-dispatch wall-clock bound. Codex uses the finite backend
    /// default when the repo has none; Claude retains its optional repo bound.
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
    ) -> Result<CodexRecovery, MemoryError> {
        Ok(CodexRecovery::NoPriorInvocation)
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

/// Codex implementation backend. Process evidence is checked before durable
/// artifacts; only trusted supervisor cleanup permits completed recovery.
pub struct CodexDispatcher {
    bin: std::path::PathBuf,
}

impl CodexDispatcher {
    pub fn resolve() -> Result<Self, MemoryError> {
        Ok(Self {
            bin: crate::launcher::find_on_path(crate::launcher::Harness::Codex.binary())?,
        })
    }
}

impl Dispatcher for CodexDispatcher {
    fn implementer(&self) -> Implementer {
        Implementer::Codex
    }

    fn recover(
        &mut self,
        _repo: &Path,
        state: &DispatchState,
    ) -> Result<CodexRecovery, MemoryError> {
        let Some(artifacts) = state.codex_artifacts.as_ref() else {
            return Ok(CodexRecovery::NoPriorInvocation);
        };
        let snapshot =
            super::codex_process::snapshot(&mut super::codex_process::UnixCodexProcesses);
        match snapshot.liveness(Some(artifacts)) {
            super::registry::Liveness::Alive => Ok(CodexRecovery::Alive),
            super::registry::Liveness::NotListed => {
                codex_dispatch::recover_artifacts(Some(artifacts))
            }
            super::registry::Liveness::Unknown => Ok(CodexRecovery::Unknown {
                reason: snapshot.invocation_reason(Some(artifacts)),
            }),
        }
    }

    fn dispatch(
        &mut self,
        repo: &Path,
        spec: &DispatchSpec,
    ) -> Result<DispatchOutcome, MemoryError> {
        codex_dispatch::run_codex(&self.bin, repo, spec)
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

/// The commit the worktree's branch currently points at, for the lineage
/// record's `commit_sha`. Best-effort: an IC that satisfied a read-only gate
/// without committing anything is a legitimate success, so an unreadable or
/// commit-less HEAD degrades to `None` rather than failing a run that
/// otherwise succeeded.
fn head_commit(worktree: &Path) -> Option<String> {
    let mut command = std::process::Command::new("git");
    // An inherited `GIT_DIR` overrides `current_dir`, so without this the
    // commit recorded as an attempt's `commit_sha` could be another
    // repository's HEAD — a wrong answer that looks exactly like a right one.
    crate::mcp::tools::scrub_git_environment(&mut command);
    let output = command
        .args(["rev-parse", "HEAD"])
        .current_dir(worktree)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
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
    recoverable_codex: bool,
) -> Result<(), MemoryError> {
    // Codex can replay durable artifacts after a crash between the append-only
    // attempt and current-status writes. Complete that same ordinal instead
    // of appending it twice. Terminal summaries are markers, not attempts.
    let existing = if recoverable_codex {
        lineage::attempts_for_issue(db, &record.issue)?
            .into_iter()
            .find(|attempt| {
                attempt.attempt_n == record.attempt_n && !is_terminal_summary(&attempt.approach)
            })
    } else {
        None
    };
    if existing
        .as_ref()
        .is_some_and(|attempt| attempt.verdict != record.verdict)
    {
        return Err(MemoryError::Validation(
            "recovered Codex verdict conflicts with the already recorded attempt ordinal".into(),
        ));
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
    let existing_state = dispatch_state::get_dispatch_state(db, issue)?;
    if let Some(state) = &existing_state {
        if state.implementer != config.implementer {
            return Err(MemoryError::Validation(format!("implementation harness mismatch: persisted {:?}, configured {:?}; resume with the persisted harness or explicitly clear its state", state.implementer, config.implementer)));
        }
    }
    let gate_commands = approved_gate_commands(db, &issue.repo)?;
    // Report the effective bound, including Codex's mandatory finite fallback.
    // Claude retains the repo's optional bound.
    let wall_clock_timeout_secs = gate_config::wall_clock_timeout(db, &issue.repo)?.or_else(|| {
        (config.implementer == Implementer::Codex)
            .then_some(codex_dispatch::CODEX_DISPATCH_TIMEOUT_SECS)
    });
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
            if config.implementer == Implementer::Codex {
                dispatch_state::clear_dispatch_state(db, issue)?;
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
    let mut codex_artifacts = existing_state
        .as_ref()
        .and_then(|s| s.codex_artifacts.clone());
    let mut recovered_outcome = None;
    if config.implementer == Implementer::Codex {
        if let Some(state) = existing_state
            .as_ref()
            .filter(|s| s.codex_artifacts.is_some())
        {
            match dispatcher.recover(&worktree.path, state)? {
                recovery @ (CodexRecovery::Alive | CodexRecovery::Unknown { .. }) => {
                    let detail = match recovery {
                        CodexRecovery::Unknown { reason } => reason,
                        _ => "Codex implementation is still alive".into(),
                    };
                    let unpriced_dispatches = match &state.codex_artifacts {
                        Some(artifacts) => u32::from(budget::has_unpriced_dispatch_reservation(
                            db,
                            &artifacts.invocation_id,
                        )?),
                        None => 0,
                    };
                    return Ok(IssueRun {
                        unpriced_dispatches,
                        issue: issue.clone(),
                        terminal: TerminalReason::ImplementationInFlight { detail },
                        dispatches: Vec::new(),
                        total_cost_usd: 0.0,
                        cumulative_attempt_n,
                        wall_clock_timeout_secs,
                    });
                }
                CodexRecovery::Finished(outcome) => {
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
                            dispatch_state::clear_dispatch_state(db, issue)?;
                            return Ok(IssueRun {
                                unpriced_dispatches: 1,
                                issue: issue.clone(),
                                terminal: TerminalReason::Impossible {
                                    reason: outcome.reason,
                                },
                                dispatches: Vec::new(),
                                total_cost_usd: 0.0,
                                cumulative_attempt_n,
                                wall_clock_timeout_secs,
                            });
                        }
                        // Preserve the proven conversation for a continuing run,
                        // but never classify or charge the same attempt again.
                        if native_session_id.is_none() {
                            native_session_id =
                                outcome.native_session_id.filter(|id| !id.trim().is_empty());
                        }
                        codex_artifacts = None;
                    } else {
                        recovered_outcome = Some(outcome);
                    }
                }
                CodexRecovery::NoPriorInvocation => {
                    return Err(MemoryError::Validation("Codex recovery returned no invocation for persisted artifacts; process completion is unproven".into()));
                }
            }
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
    let write_state = |db: &Database,
                       attempt_n: u32,
                       turn_n: u32,
                       state: &str,
                       session_uuid: &str,
                       session_claimed: bool,
                       identity: (&Option<String>, &Option<CodexArtifacts>)| {
        dispatch_state::upsert_dispatch_state(
            db,
            &DispatchState {
                implementer: config.implementer,
                native_session_id: if config.implementer == Implementer::Claude {
                    Some(session_uuid.to_string())
                } else {
                    identity.0.clone()
                },
                codex_artifacts: identity.1.clone(),
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
            },
        )
        .map(|_| ())
    };

    if config.implementer == Implementer::Claude {
        write_state(
            db,
            cumulative_attempt_n,
            turn_n,
            "dispatching",
            &session_uuid,
            resuming,
            (&native_session_id, &codex_artifacts),
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
            // Codex always has a finite bound and reserves unknown spend.
            // Claude keeps its existing policy: this cap gates only repos
            // with a wall-clock bound.
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
                        db,
                        cumulative_attempt_n,
                        turn_n,
                        "paused-condition-too-long",
                        &session_uuid,
                        resuming,
                        (&native_session_id, &codex_artifacts),
                    )?;
                    return Err(MemoryError::Validation(e.to_string()));
                }
            };

            let prepared_artifacts = if config.implementer == Implementer::Codex {
                Some(codex_dispatch::prepare_artifacts(&worktree.path, issue)?)
            } else {
                codex_artifacts.clone()
            };
            let spec = DispatchSpec {
                implementer: config.implementer,
                codex_artifacts: prepared_artifacts,
                session: if config.implementer == Implementer::Codex {
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

            if config.implementer == Implementer::Codex {
                // Validate the exact backend argv before publishing a new
                // invocation or charging unknown spend. Definite input errors
                // prove no process launched and must leave prior state intact.
                codex_dispatch::build_argv(&worktree.path, &spec)?;
                if let Some(artifacts) = &spec.codex_artifacts {
                    codex_artifacts = Some(artifacts.clone());
                    write_state(
                        db,
                        cumulative_attempt_n,
                        turn_n,
                        "dispatching",
                        &session_uuid,
                        resuming,
                        (&native_session_id, &codex_artifacts),
                    )?;
                    budget::reserve_unpriced_dispatch(db, &today_utc(), &artifacts.invocation_id)?;
                    unpriced_dispatches += 1;
                }
            }
            dispatcher.dispatch(&worktree.path, &spec)
        };
        let mut outcome = match outcome_result {
            Ok(outcome) => outcome,
            Err(err) => {
                if config.implementer == Implementer::Codex {
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
                        detail: format!("Codex dispatch could not establish completion: {err}"),
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
                    db,
                    cumulative_attempt_n,
                    turn_n,
                    "infrastructure-failure",
                    &session_uuid,
                    resuming,
                    (&native_session_id, &codex_artifacts),
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
        if config.implementer == Implementer::Codex {
            if recovered {
                unpriced_dispatches += 1;
            }
            if outcome
                .native_session_id
                .as_ref()
                .is_none_or(|id| id.trim().is_empty())
                || native_session_id
                    .as_ref()
                    .is_some_and(|id| outcome.native_session_id.as_ref() != Some(id))
            {
                outcome.native_session_id = None;
                outcome.verdict = None;
                outcome.is_error = true;
                outcome.process_success = false;
                outcome.reason = Some(
                    "missing or mismatched native Codex session ID; persisted identity retained"
                        .into(),
                );
            } else if native_session_id.is_none() {
                native_session_id = outcome.native_session_id.clone();
            }
            codex_artifacts = None;
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
        if config.implementer == Implementer::Claude && outcome.verdict.is_some() {
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
        if config.implementer == Implementer::Codex {
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
                let commit_sha = head_commit(&worktree.path);
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
                    config.implementer == Implementer::Codex,
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
                    config.implementer == Implementer::Codex,
                )?;
                break TerminalReason::Impossible {
                    reason: outcome.reason.clone(),
                };
            }
            DispatchClassification::FailedAttempt => {
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
                    config.implementer == Implementer::Codex,
                )?;
                status = lineage::get_issue_status(db, issue)?;
                write_state(
                    db,
                    cumulative_attempt_n,
                    turn_n,
                    "dispatching",
                    &session_uuid,
                    resuming,
                    (&native_session_id, &codex_artifacts),
                )?;
            }
            DispatchClassification::InfrastructureFailure => {
                write_state(
                    db,
                    cumulative_attempt_n,
                    turn_n,
                    "infrastructure-failure",
                    &session_uuid,
                    resuming,
                    (&native_session_id, &codex_artifacts),
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
        dispatch_state::clear_dispatch_state(db, issue)?;
    } else if !matches!(terminal, TerminalReason::ImplementationInFlight { .. }) {
        let state_label = match &terminal {
            TerminalReason::DailyBudgetExhausted { .. } => "paused-daily-budget",
            _ => "paused-infrastructure-failure",
        };
        write_state(
            db,
            cumulative_attempt_n,
            turn_n,
            state_label,
            &session_uuid,
            resuming,
            (&native_session_id, &codex_artifacts),
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

    #[test]
    fn invalid_codex_resume_refuses_before_reservation_or_state_overwrite() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut state = pending_codex(&db, &wt, Some("invalid-native-uuid"));
        state.codex_artifacts = None;
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
            CodexRecovery::Alive,
            CodexRecovery::Unknown {
                reason: "unproven completion".into(),
            },
        ] {
            for reserved_date in [None, Some(today_utc()), Some("2026-01-01".into())] {
                let db = approved_db();
                let (_dir, wt) = fixture_worktree();
                let state = pending_codex(&db, &wt, None);
                let id = &state.codex_artifacts.as_ref().unwrap().invocation_id;
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
        d.recovery = Some(CodexRecovery::Finished(result));
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
        let (repo, _) = fixture_worktree();
        let roots = tempfile::tempdir().unwrap();
        let first = worktree::ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        let state = pending_codex(&db, &first, None);
        std::fs::write(first.path.join("dirty.txt"), "unfinished").unwrap();
        let fresh = worktree::ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        assert!(fresh.quarantined_from.is_some());
        let artifacts = state.codex_artifacts.as_ref().unwrap();
        assert!(Path::new(&artifacts.journal_path).is_file());
        let mut d = codex_script(vec![]);
        d.recovery = Some(CodexRecovery::Alive);
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
                &state.codex_artifacts.as_ref().unwrap().invocation_id,
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
                true,
            )
            .unwrap();
            let attempts = lineage::attempts_for_issue(&db, &issue()).unwrap();
            let status = lineage::get_issue_status(&db, &issue()).unwrap();
            let mut d = codex_script(vec![met()]);
            d.recovery = Some(CodexRecovery::Finished(result.clone()));
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
                &state.codex_artifacts.as_ref().unwrap().invocation_id,
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
                true,
            )
            .unwrap();
            result.cost_usd = None;
            let mut d = codex_script(vec![met()]);
            d.recovery = Some(CodexRecovery::Finished(result));
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
        d.recovery = Some(CodexRecovery::Finished(result));
        let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
        assert!(matches!(run.terminal, TerminalReason::AttemptCapExhausted));
        assert_eq!(
            run.cumulative_attempt_n, 1,
            "a classified artifact must not spend the attempt twice"
        );
        assert!(run.dispatches.is_empty());
        assert!(d.inner.seen.is_empty());
        assert!(state.codex_artifacts.unwrap().journal_path.starts_with('/'));
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
"#,
        )
        .unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut d = CodexDispatcher { bin };
        let state = pending_codex(&db, &wt, None);
        assert!(matches!(
            d.recover(&wt.path, &state).unwrap(),
            CodexRecovery::Unknown { .. }
        ));
        std::fs::write(
            &state.codex_artifacts.as_ref().unwrap().exit_path,
            "not-json",
        )
        .unwrap();
        assert!(matches!(
            d.recover(&wt.path, &state).unwrap(),
            CodexRecovery::Unknown { .. }
        ));
        dispatch_state::clear_dispatch_state(&db, &issue()).unwrap();
        let run = run_issue(&db, &issue(), &brief(), &wt, &codex_config(), &mut d).unwrap();
        assert!(matches!(run.terminal, TerminalReason::Met { .. }));
        assert_eq!(run.dispatches[0].total_cost_usd, None);
        assert_eq!(run.unpriced_dispatches, 1);
        assert_eq!(run.total_cost_usd, 0.0);
    }
    struct CodexScript {
        inner: ScriptedDispatcher,
        recovery: Option<super::super::codex_dispatch::CodexRecovery>,
    }
    impl Dispatcher for CodexScript {
        fn implementer(&self) -> Implementer {
            Implementer::Codex
        }
        fn recover(
            &mut self,
            _repo: &Path,
            _state: &DispatchState,
        ) -> Result<CodexRecovery, MemoryError> {
            Ok(self
                .recovery
                .take()
                .unwrap_or(CodexRecovery::NoPriorInvocation))
        }
        fn dispatch(
            &mut self,
            repo: &Path,
            spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            self.inner.dispatch(repo, spec)
        }
    }
    fn codex_script(outcomes: Vec<DispatchOutcome>) -> CodexScript {
        CodexScript {
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
    fn codex_config() -> RunConfig {
        let mut c = config();
        c.implementer = Implementer::Codex;
        c.model = "gpt-6.1-sol".into();
        c
    }
    fn pending_codex(db: &Database, worktree: &Worktree, native: Option<&str>) -> DispatchState {
        let artifacts =
            super::super::codex_dispatch::prepare_artifacts(&worktree.path, &issue()).unwrap();
        let state = DispatchState {
            implementer: Implementer::Codex,
            native_session_id: native.map(str::to_string),
            codex_artifacts: Some(artifacts),
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
            d.inner.seen[0].codex_artifacts,
            d.inner.seen[1].codex_artifacts
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
            CodexRecovery::Alive,
            CodexRecovery::Unknown {
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
        d.recovery = Some(CodexRecovery::Finished(result));
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
            d.recovery = Some(CodexRecovery::Finished(result));
            let mut c = codex_config();
            c.max_consecutive_infrastructure_failures = 1;
            let run = run_issue(&db, &issue(), &brief(), &wt, &c, &mut d).unwrap();
            assert!(matches!(
                run.terminal,
                TerminalReason::InfrastructureFailure { .. }
            ));
            assert!(d.inner.seen.is_empty());
            let state = dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .unwrap();
            assert_eq!(state.native_session_id.as_deref(), Some("persisted"));
            assert!(state.codex_artifacts.is_none());
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
    /// `head_commit` has a genuine SHA to read rather than a fabricated one.
    fn fixture_worktree() -> (tempfile::TempDir, Worktree) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().to_path_buf();
        for args in [
            vec!["init", "--initial-branch=main"],
            vec!["config", "user.email", "autopilot@example.test"],
            vec!["config", "user.name", "Autopilot Test"],
        ] {
            let ok = std::process::Command::new("git")
                .args(&args)
                .current_dir(&path)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        }
        std::fs::write(path.join("README.md"), "seed\n").unwrap();
        for args in [vec!["add", "README.md"], vec!["commit", "-m", "seed"]] {
            std::process::Command::new("git")
                .args(&args)
                .current_dir(&path)
                .output()
                .unwrap();
        }
        let wt = Worktree {
            path: path.clone(),
            branch: worktree::branch_name(&issue()),
            created: true,
            quarantined_from: None,
        };
        (dir, wt)
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
        assert!(config.validate().is_err());
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
            false,
        )
        .unwrap();

        let after = lineage::get_issue_status(&db, &issue()).unwrap().unwrap();
        assert_eq!(after.best_verdict, Some(AttemptOutcome::Success));
        assert_eq!(after.best_commit_sha, succeeded.best_commit_sha);
        assert_eq!(after.cumulative_attempt_n, 2);
    }
    #[test]
    fn completed_codex_pauses_reenter_queue_and_resume_native_identity_once() {
        use crate::autopilot::codex_process::CodexProcessSnapshot;
        use crate::autopilot::gh::IssueListing;
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
                    state.codex_artifacts, spec.codex_artifacts,
                    "pending invocation is durable before launch"
                );
                self.invocation =
                    Some(spec.codex_artifacts.as_ref().unwrap().invocation_id.clone());
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
            assert!(state.codex_artifacts.is_none());
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
                codex: CodexProcessSnapshot::Available(Vec::new()),
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
            resumed.recovery = Some(CodexRecovery::Unknown {
                reason: "completed invocation must not be recovered again".into(),
            });
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
                    .codex_artifacts
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
