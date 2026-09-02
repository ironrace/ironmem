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
//!    `n_turns` by [`MIN_MAX_TURNS_HEADROOM`] — closing rung 2's flagged
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

use super::dispatch::{self, DispatchOutcome, DispatchSpec, SessionMode, Verdict};
use super::worktree::Worktree;
use super::{
    blocked, budget, dispatch_state, gate_config, lineage, supervise, today_utc, turn_prompt,
    IssueRef,
};
use super::{AttemptOutcome, AttemptRecord, DispatchState, IssueStatus, PriorAttempt};

/// Turns per dispatch (the `N` in the spec's `/goal <gates> or stop after N
/// turns`). The spec's *IC lifecycle* section gives 5–8 as an explicitly
/// unmeasured starting suggestion and rung 0 declined to narrow it, because
/// its probe tasks were too light to observe the supervision-vs-cost tradeoff
/// that actually determines it. This is the midpoint of that range, not a
/// measurement.
pub const DEFAULT_N_TURNS: u32 = 6;

/// Minimum amount by which `max_turns` must exceed `n_turns`.
///
/// Derived from the one real measurement there is. Rung 2 dispatched with
/// `/goal ... or stop after 1 turns` and `--max-turns 4`; the goal loop
/// needed 5 turns to reach its own evaluated stop, so the CLI's hard cap
/// fired first and the dispatch returned `is_error: true` with no
/// `structured_output` — despite having completed the work correctly on
/// disk. Four turns of overhead above `N` is what that single data point
/// showed, so four is the floor. It is a floor from one observation, not a
/// law; [`DEFAULT_MAX_TURNS_HEADROOM`] leaves more room than the floor
/// requires for exactly that reason.
pub const MIN_MAX_TURNS_HEADROOM: u32 = 4;

/// Headroom [`RunConfig::new`] applies over `n_turns` when the caller does
/// not set `max_turns` explicitly.
pub const DEFAULT_MAX_TURNS_HEADROOM: u32 = 6;

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
/// **The counter is shared with [`super::review`]'s unpriced reviews**, since
/// both are unpriced invocations against one day's ledger. Each consumes the
/// other's headroom, which is conservative in both directions — the effective
/// bound is the smaller of the two — but it is a real coupling, so it is said
/// out loud here rather than discovered.
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
            model: model.into(),
            dispatch_class: dispatch_class.into(),
            n_turns: DEFAULT_N_TURNS,
            max_turns: DEFAULT_N_TURNS + DEFAULT_MAX_TURNS_HEADROOM,
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
    /// [`MIN_MAX_TURNS_HEADROOM`]. The budget check is the other: a
    /// `max_budget_usd` above `daily_budget_usd` means the pre-authorization
    /// in [`run_issue`] can never clear, so the run would report a budget
    /// stop without ever dispatching — a misconfiguration worth naming at
    /// the point it is made rather than diagnosing from a silent no-op.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if self.model.trim().is_empty() {
            return Err(MemoryError::Validation("model must not be empty".into()));
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
        if self.max_turns < self.n_turns.saturating_add(MIN_MAX_TURNS_HEADROOM) {
            return Err(MemoryError::Validation(format!(
                "max_turns ({}) must exceed n_turns ({}) by at least {} — a hard --max-turns cap \
                 that fires before the goal loop's own stop suppresses the verdict schema \
                 entirely, so a completed dispatch returns as an unrecorded failure",
                self.max_turns, self.n_turns, MIN_MAX_TURNS_HEADROOM
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
    DispatchClassification::FailedAttempt
}

/// Why the run stopped.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(tag = "terminal", rename_all = "snake_case")]
pub enum TerminalReason {
    /// The gate condition was satisfied. Terminal for the issue: the
    /// dispatch-state drawer is cleared and the branch is ready for the
    /// Reviewer (rung 5).
    Met { commit_sha: Option<String> },
    /// A prior run already recorded a success for this issue, so nothing was
    /// dispatched. Makes [`run_issue`] idempotent — re-running a finished
    /// issue costs nothing rather than re-doing settled work.
    AlreadySucceeded { commit_sha: Option<String> },
    /// The per-issue attempt cap was reached. Terminal: per the spec,
    /// `agent:exhausted` *"never self-resumes"*, so the dispatch state is
    /// cleared and only a human re-labeling retries.
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
            TerminalReason::DailyBudgetExhausted { .. }
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
    pub total_cost_usd: f64,
    pub num_turns: u32,
    pub verdict: Option<Verdict>,
    pub reason: Option<String>,
}

/// The result of driving one issue.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct IssueRun {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub terminal: TerminalReason,
    pub dispatches: Vec<DispatchSummary>,
    /// Spend this run added to the daily ledger — the sum of every dispatch
    /// that returned a parseable result, successful or not.
    pub total_cost_usd: f64,
    /// The issue's cumulative attempt count after the run.
    pub cumulative_attempt_n: u32,
    /// The repo's per-dispatch wall-clock bound, or `None` if it has none.
    ///
    /// Reported rather than merely applied: an unbounded repo is a real
    /// operational fact (a wedged dispatch there runs forever), and a caller
    /// that never sees the `None` cannot tell an unbounded repo from a
    /// bounded one that simply never timed out.
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
    let output = std::process::Command::new("git")
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
            outcome.num_turns, outcome.session_id
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
) -> Result<(), MemoryError> {
    lineage::record_attempt(db, record)?;

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
pub(super) fn is_terminal_summary(approach: &str) -> bool {
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
/// carried separately only for the message, since a cap lowered between runs
/// leaves `attempt_n` above it.
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
    if attempts.iter().any(|a| is_terminal_summary(&a.approach)) {
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

    let gate_commands = approved_gate_commands(db, &issue.repo)?;
    // Read *after* the approval check above, which is the gate that refuses
    // an unapproved repo outright. `None` here means this repo has no
    // wall-clock bound — legal, and the pre-rung-7 behavior, but it is
    // surfaced on `IssueRun` rather than passing silently, because "no
    // timeout" and "a timeout nobody set" look identical from the outside.
    let wall_clock_timeout_secs = gate_config::wall_clock_timeout(db, &issue.repo)?;
    let wall_clock_timeout = wall_clock_timeout_secs.map(std::time::Duration::from_secs);

    let mut status = lineage::get_issue_status(db, issue)?;
    if let Some(existing) = &status {
        if existing.best_verdict == Some(AttemptOutcome::Success) {
            return Ok(IssueRun {
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

    // Resume the issue's existing session if one is in flight; otherwise open
    // a new one. The spec's crash-safe-state section is why the session uuid
    // lives in the drawer rather than in the caller's memory: "any Lead can
    // resume any IC".
    let existing_state = dispatch_state::get_dispatch_state(db, issue)?;
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

    let write_state =
        |db: &Database, attempt_n: u32, turn_n: u32, state: &str, session_claimed: bool| {
            dispatch_state::upsert_dispatch_state(
                db,
                &DispatchState {
                    issue: issue.clone(),
                    worktree_path: worktree_path.clone(),
                    ic_session_name: ic_session_name.clone(),
                    dispatch_class: config.dispatch_class.clone(),
                    attempt_n,
                    state: state.to_string(),
                    started_at: started_at.clone(),
                    session_uuid: session_uuid.clone(),
                    turn_n,
                    session_claimed,
                },
            )
            .map(|_| ())
        };

    write_state(db, cumulative_attempt_n, turn_n, "dispatching", resuming)?;

    let mut dispatches = Vec::new();
    let mut total_cost_usd = 0.0;
    let mut consecutive_infrastructure_failures = 0u32;

    let terminal = loop {
        // Rung 7's escalation, enforced rather than merely reported. Checked
        // inside the loop, not once before it: a supervisor polling
        // alongside a long run can escalate mid-run, and the next dispatch
        // must see it.
        if let Some(signature) = supervise::escalated_signature(db, issue)? {
            break TerminalReason::StrategyEscalated { signature };
        }
        if cumulative_attempt_n >= config.attempt_cap {
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
        // The second ceiling. Checked only when this repo can actually
        // produce an unpriced dispatch: without a wall-clock bound no
        // dispatch here can time out, so unpriced invocations banked by
        // *other* work (a reviewer, another repo's timeouts) must not stop a
        // repo that cannot contribute to the counter.
        if wall_clock_timeout.is_some() {
            let unpriced_today = ledger
                .as_ref()
                .map(|e| e.unpriced_dispatch_count)
                .unwrap_or(0);
            if unpriced_today >= config.max_unpriced_dispatches_per_day {
                break TerminalReason::UnpricedDispatchesExhausted { unpriced_today };
            }
        }

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
        let condition = turn_prompt::render(&turn_prompt::TurnPromptInputs {
            issue,
            issue_title: &brief.title,
            issue_body: &brief.body,
            prior_attempts: &attempts,
            strategy_redirect: strategy_redirect.as_deref(),
            human_answers: &human_answers,
            gate_commands: &gate_commands,
            n_turns: config.n_turns,
        });

        let spec = DispatchSpec {
            session: if resuming {
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

        let outcome = match dispatcher.dispatch(&worktree.path, &spec) {
            Ok(outcome) => outcome,
            Err(err) => {
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
                    total_cost_usd: 0.0,
                    num_turns: 0,
                    verdict: None,
                    reason: Some(err.to_string()),
                });
                write_state(
                    db,
                    cumulative_attempt_n,
                    turn_n,
                    "infrastructure-failure",
                    resuming,
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

        // The session exists from here on regardless of how this dispatch
        // lands: `--session-id` may only be used once, so every subsequent
        // invocation must be a `--resume`.
        resuming = true;
        turn_n = turn_n.saturating_add(outcome.num_turns);

        // Bank spend before deciding anything else. Every dispatch that
        // returned a result is billed, met or not.
        //
        // A dispatch killed on the wall-clock bound is the exception, and
        // the reason rung 5's unpriced counter exists: it really did spend
        // money, and its result JSON — the only meter there is — died with
        // it. Banking its synthesized `0.0` as *spend* would record a
        // measurement that was never taken; banking it as *unpriced* marks
        // the day's total as a floor, which is the true statement.
        if outcome.timed_out {
            budget::record_unpriced_dispatch(db, &today_utc())?;
        } else {
            budget::accumulate_daily_spend(db, &today_utc(), outcome.total_cost_usd.max(0.0))?;
            total_cost_usd += outcome.total_cost_usd.max(0.0);
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
            total_cost_usd: outcome.total_cost_usd,
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
                )?;
                status = lineage::get_issue_status(db, issue)?;
                write_state(db, cumulative_attempt_n, turn_n, "dispatching", resuming)?;
            }
            DispatchClassification::InfrastructureFailure => {
                write_state(
                    db,
                    cumulative_attempt_n,
                    turn_n,
                    "infrastructure-failure",
                    resuming,
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
    } else {
        let state_label = match &terminal {
            TerminalReason::DailyBudgetExhausted { .. } => "paused-daily-budget",
            _ => "paused-infrastructure-failure",
        };
        write_state(db, cumulative_attempt_n, turn_n, state_label, resuming)?;
    }

    Ok(IssueRun {
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
    use crate::autopilot::worktree;

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
            total_cost_usd: cost,
            num_turns: 3,
            duration_ms: 1_000,
            is_error,
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
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
    /// placeholder — `total_cost_usd: 0.0` means *unknown*, not *free*.
    fn timed_out() -> DispatchOutcome {
        DispatchOutcome {
            total_cost_usd: 0.0,
            num_turns: 0,
            duration_ms: 600_000,
            is_error: true,
            session_id: "11111111-2222-3333-4444-555555555555".to_string(),
            verdict: None,
            reason: Some("dispatch exceeded this repo's wall-clock bound".to_string()),
            process_success: false,
            timed_out: true,
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
        // unpriced invocations banked elsewhere — a Codex reviewer, another
        // repo's timeouts, which share this counter — must not stop it.
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
        assert!(config.max_turns >= config.n_turns + MIN_MAX_TURNS_HEADROOM);
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
    fn the_first_dispatch_opens_a_session_and_later_ones_resume_it() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(not_met()), Ok(met())]);

        run_issue(&db, &issue(), &brief(), &wt, &config(), &mut dispatcher).unwrap();

        assert_eq!(dispatcher.seen.len(), 2);
        let uuid = match &dispatcher.seen[0].session {
            SessionMode::New { session_uuid } => session_uuid.clone(),
            other => panic!("first dispatch must open a new session, got {other:?}"),
        };
        match &dispatcher.seen[1].session {
            SessionMode::Resume { session_uuid } => assert_eq!(session_uuid, &uuid),
            other => panic!("later dispatches must resume, got {other:?}"),
        }
    }

    #[test]
    fn a_resumed_run_reuses_the_stored_session_uuid() {
        let db = approved_db();
        let (_dir, wt) = fixture_worktree();
        // First run stops on the daily budget, leaving the state in place.
        let mut paused = config();
        paused.daily_budget_usd = 2.6;
        paused.max_budget_usd = 2.5;
        let mut dispatcher = ScriptedDispatcher::new(vec![Ok(not_met())]);
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
        let mut first = ScriptedDispatcher::new(vec![Ok(not_met())]);
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
        )
        .unwrap();

        let after = lineage::get_issue_status(&db, &issue()).unwrap().unwrap();
        assert_eq!(after.best_verdict, Some(AttemptOutcome::Success));
        assert_eq!(after.best_commit_sha, succeeded.best_commit_sha);
        assert_eq!(after.cumulative_attempt_n, 2);
    }
}
