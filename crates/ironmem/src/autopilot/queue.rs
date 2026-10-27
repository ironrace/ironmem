//! The cross-repo queue — build-ladder rung 8.
//!
//! The spec's data flow opens with three lines this module is all of:
//!
//! ```text
//! agent:ready issues (all write-access repos)
//!   └─► Lead: pick by priority:* order; check budget, concurrency cap,
//!        per-issue attempt cap
//! ```
//!
//! Every earlier rung took the issue it was told to work on. [`plan_queue`]
//! is the first code in the ladder that *chooses*, and it is deliberately a
//! pure function over already-fetched GitHub listings plus the database:
//! [`lead`](super::lead) does the fetching and the dispatching, so the
//! choosing can be tested exhaustively against a real database without a
//! single GitHub call.
//!
//! # Measured before written
//!
//! Rung 7's lesson 20 — measure the response shape when measuring is free —
//! applied again, and again it changed the code. `gh label list` on this
//! repo (free, read-only, 2026-09-02) says the `priority:*` vocabulary is
//! exactly three labels: `priority:high`, `priority:medium`, `priority:low`.
//! There is no `priority:critical` or `priority:p0`, which is what
//! [`Priority`] would otherwise have been written against. The same read
//! confirmed a second fact worth stating: **none of the three `agent:*`
//! labels exist on this repo yet** — `ironmem autopilot labels` (rung 6) has
//! never been run against it, so an unprimed repo's backlog is legitimately
//! empty rather than broken.
//!
//! # Ordering is not a safety boundary
//!
//! Priority decides *what gets picked first*, and nothing else. Every guard
//! that can actually cause harm — repo approval, label eligibility, the
//! attempt cap, a strategy escalation, a live session, the concurrency cap,
//! the daily budget — is evaluated independently of order and cannot be
//! affected by it. That is what lets [`Priority::of`] resolve conflicting
//! labels toward the **highest** one, the natural reading of "this is also
//! P0", rather than toward the restrictive answer rung 6's
//! [`eligibility`](super::labels::eligibility) is obliged to take. A stray
//! `priority:high` can move an issue up the list; it can never start work
//! that was not already authorized.
//!
//! # Two things the cap must not confuse
//!
//! An issue with a dispatch-state drawer is **in flight** — it holds one of
//! the concurrency cap's slots whether or not its IC process is currently
//! alive, because an IC exits at the end of every dispatch by design and the
//! session is the durable thing, not the process. Such an issue is also the
//! best thing to pick next: resuming a session parked on yesterday's budget
//! costs ~5% of a cold start (rung 0's measurement), and leaving it parked
//! while starting fresh work is how a fleet of half-finished issues
//! accumulates. So in-flight issues sort **ahead** of everything else and
//! re-occupy the slot they already hold.
//!
//! The one thing that must not happen is a second `run_issue` on an issue
//! whose IC is alive *right now*: two dispatches sharing one worktree, which
//! is the collision rung 7 refused to risk when it declined to restart a
//! listed-but-stalled session. Hence [`DeferReason::SessionLive`].
//!
//! # What an unreadable registry may and may not stop
//!
//! Rung 7's lesson 21 says an unreadable collection must not degrade to an
//! empty one. Applied literally here it would stop the Lead entirely
//! whenever `claude agents --json` hiccups, which is too much: the hazard a
//! live session creates is *specific* to an issue that already has one.
//!
//! So the answer is split by what the registry is actually being asked:
//!
//! - An issue with **no** dispatch-state drawer has no session to collide
//!   with, whatever the registry says. It dispatches.
//! - An issue **with** one is deferred [`DeferReason::RegistryUnreadable`]
//!   until the registry can be read.
//!
//! The concurrency cap itself never consults the registry — it counts
//! drawers — so it is unaffected either way.

use serde::Serialize;

use super::gh::IssueListing;
use super::implementer::Implementer;
use super::labels::{self, DispatchEligibility};
#[cfg(test)]
use super::registry::RegistrySnapshot;
use super::registry::{Liveness, LivenessSnapshot};
use super::{
    budget, dispatch_state, gate_config, lineage, merge::serialize_issue, remediate, supervise,
    today_utc, validate_repo, AttemptOutcome, IssueRef,
};
use crate::db::schema::Database;
use crate::error::MemoryError;

/// Default ceiling on concurrently in-flight ICs.
///
/// No spec basis — open question 11 leaves the number to implementation.
/// Three is chosen to be small enough that a first real run is watchable and
/// its worst-case spend is `3 × max_budget_usd` per dispatch round, and is
/// meant to be raised by an operator who has watched one.
pub const DEFAULT_CONCURRENCY_CAP: usize = 3;

/// Default `--limit` for each repo's `gh issue list`.
///
/// A ceiling on one API response, not on the backlog: an issue beyond it is
/// simply not seen this pass and will be on the next one. Deliberately well
/// above [`DEFAULT_CONCURRENCY_CAP`] so priority ordering has a real
/// population to order.
pub const DEFAULT_MAX_ISSUES_PER_REPO: u32 = 50;

/// Where an issue sits in the `priority:*` ordering.
///
/// The three named variants are the real, measured label vocabulary of this
/// repo (see the module doc). [`Priority::Unlabeled`] is the fourth state
/// every ordering needs and no label spells.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Below `priority:low`? No — **above** it.
    ///
    /// `priority:low`'s own description on this repo is *"Optional / tier-2 —
    /// do when the rest lands"*, which is a deliberate deprioritization. An
    /// issue nobody has triaged has not been deprioritized; it has not been
    /// judged. Sorting the untriaged below the explicitly-optional would let
    /// a backlog of ordinary work sit behind everything anyone ever marked
    /// as skippable.
    Low,
    Unlabeled,
    Medium,
    High,
}

impl Priority {
    /// The `priority:*` label this corresponds to, or `None` for
    /// [`Priority::Unlabeled`].
    pub fn label(self) -> Option<&'static str> {
        match self {
            Priority::High => Some("priority:high"),
            Priority::Medium => Some("priority:medium"),
            Priority::Low => Some("priority:low"),
            Priority::Unlabeled => None,
        }
    }

    /// Read an issue's priority from its labels, highest wins.
    ///
    /// Case-insensitive, for the reason rung 6 learned the hard way: GitHub
    /// label names are case-insensitive, so a hand-created `Priority:High`
    /// is the same label to GitHub and would otherwise read as foreign here.
    ///
    /// # Why the seed is `None` rather than [`Priority::Unlabeled`]
    ///
    /// Because `Unlabeled` deliberately *outranks* `Low`. Folding "highest
    /// wins" over a `Priority::Unlabeled` seed makes `priority:low`
    /// unrepresentable: the label is read, loses the comparison to the seed,
    /// and the issue reports as untriaged — silently promoting every
    /// explicitly-deprioritized issue. "Highest of the labels present, or
    /// unlabeled if there are none" is the rule; the seed has to say
    /// *absent*, not *a rank*.
    pub fn of(labels: &[String]) -> Self {
        let mut best: Option<Priority> = None;
        for label in labels {
            let normalized = label.trim().to_ascii_lowercase();
            let found = match normalized.as_str() {
                "priority:high" => Priority::High,
                "priority:medium" => Priority::Medium,
                "priority:low" => Priority::Low,
                _ => continue,
            };
            if best.is_none_or(|current| found > current) {
                best = Some(found);
            }
        }
        best.unwrap_or(Priority::Unlabeled)
    }
}

/// One repo's `agent:ready` listing, as [`super::lead`] fetched it.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoBacklog {
    pub repo: String,
    pub issues: Vec<IssueListing>,
}

/// Policy knobs for one queue pass.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueConfig {
    pub concurrency_cap: usize,
    pub daily_budget_usd: f64,
    /// The per-dispatch ceiling [`super::run::RunConfig::max_budget_usd`]
    /// will use.
    ///
    /// The queue needs it because `run_issue` **pre-authorizes** spend
    /// (`spent + max_budget_usd > daily_budget_usd` stops it) rather than
    /// checking the total after the fact. A queue that admitted on
    /// `spent >= daily_budget_usd` would hand `run_issue` an issue it
    /// refuses on the very next line, and report a dispatch that never
    /// happened — this ladder's "reports without binding" shape, which rung
    /// 7's review found three times in one rung. Same predicate, one place
    /// each.
    pub max_budget_usd: f64,
    /// Compared against [`super::remediate::attempts_charged`] — attempts
    /// since the last human retry or, for a remediation, since the issue
    /// first succeeded — the same operand
    /// [`super::run::RunConfig::attempt_cap`] is compared against. Checked here as well as in
    /// `run_issue` so an exhausted issue is reported as such by the queue
    /// rather than dispatched and immediately terminated.
    pub attempt_cap: u32,
    pub max_issues_per_repo: u32,
    /// The harness the dispatching runner will use. A state persisted by the
    /// other harness is deferred rather than admitted as a resume, because the
    /// runner refuses it — and a refused resume sorted to the top would take
    /// the tick's dispatch every tick. `None` is an unscoped preview.
    pub implementer: Option<Implementer>,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            concurrency_cap: DEFAULT_CONCURRENCY_CAP,
            daily_budget_usd: super::run::DEFAULT_DAILY_BUDGET_USD,
            max_budget_usd: super::run::DEFAULT_MAX_BUDGET_USD,
            attempt_cap: super::run::DEFAULT_ATTEMPT_CAP,
            max_issues_per_repo: DEFAULT_MAX_ISSUES_PER_REPO,
            implementer: None,
        }
    }
}

impl QueueConfig {
    /// Reject a configuration that cannot produce a usable plan.
    ///
    /// The `is_finite` check is rung 5's finding #3 carried over rather than
    /// re-learned: `spent >= NaN` is false forever, so a NaN budget does not
    /// fail closed — it silently removes the ceiling. Rung 7's lesson 26 is
    /// that reusing a mechanism means inheriting its fix.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if self.concurrency_cap == 0 {
            return Err(MemoryError::Validation(
                "concurrency_cap must be at least 1 — a cap of zero dispatches nothing, \
                 ever, which is a stopped Lead spelled as a configuration"
                    .into(),
            ));
        }
        if !self.daily_budget_usd.is_finite() || self.daily_budget_usd <= 0.0 {
            return Err(MemoryError::Validation(format!(
                "daily_budget_usd must be a finite positive number, got {}",
                self.daily_budget_usd
            )));
        }
        if !self.max_budget_usd.is_finite() || self.max_budget_usd <= 0.0 {
            return Err(MemoryError::Validation(format!(
                "max_budget_usd must be a finite positive number, got {}",
                self.max_budget_usd
            )));
        }
        if self.max_budget_usd > self.daily_budget_usd {
            return Err(MemoryError::Validation(format!(
                "max_budget_usd ({}) exceeds daily_budget_usd ({}) — the pre-authorization \
                 could never clear, so the queue would defer every issue on budget without \
                 a dispatch ever being possible",
                self.max_budget_usd, self.daily_budget_usd
            )));
        }
        if self.attempt_cap == 0 {
            return Err(MemoryError::Validation(
                "attempt_cap must be at least 1".into(),
            ));
        }
        if self.max_issues_per_repo == 0 {
            return Err(MemoryError::Validation(
                "max_issues_per_repo must be at least 1".into(),
            ));
        }
        Ok(())
    }
}

/// An issue the queue selected, in the order it should be worked.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueuedIssue {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub title: String,
    #[serde(skip)]
    pub body: String,
    pub priority: Priority,
    /// Whether this issue already has a dispatch-state drawer, and is
    /// therefore a session to resume rather than one to open.
    pub resuming: bool,
    pub cumulative_attempt_n: u32,
    /// The value of the issue's `risk:<class>` label, if it carries one.
    ///
    /// The *label read* is a mechanical fact and belongs here. Deciding what
    /// to do when there is no label — which class an unjudged issue
    /// dispatches as — is a Lead policy call, and lives in
    /// [`super::lead::class_for`]. Keeping the fallback out of the queue is
    /// what lets `plan_queue` stay a pure function of the world rather than
    /// of a run configuration.
    pub risk_label: Option<String>,
}

/// Why an issue in a backlog was not selected.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum DeferReason {
    /// The repo has no approved gate config, so nothing in it is
    /// dispatchable. `run_issue` refuses too; naming it here means the
    /// operator reads "onboard this repo" instead of a per-issue error.
    RepoNotApproved,
    /// Rung 6's label eligibility said no.
    NotEligible { eligibility: DispatchEligibility },
    /// Lineage already records a success, and no reviewer has asked for
    /// changes to it.
    ///
    /// Both halves matter. This was the queue's one permanent dead end until
    /// rung 10 gave it an exit forward (`autopilot advance` reviews and merges
    /// the PR) and rung 11 gave it an exit back
    /// ([`super::remediate::active_remediation`] re-opens it on a
    /// `needs_changes` verdict). A succeeded issue with a remediation in force
    /// is **not** deferred here — it is dispatched, under the same attempt cap
    /// value as any other work, counted from the issue's first success.
    AlreadySucceeded,
    /// The per-issue attempt cap is spent. `cumulative_attempt_n` is the
    /// lifetime count, so it can exceed the cap after a retry or a
    /// remediation; what reached the cap is
    /// [`super::remediate::attempts_charged`].
    AttemptCapReached { cumulative_attempt_n: u32 },
    /// Rung 7's strategy-health escalated this issue; only a human clears it.
    StrategyEscalated { signature: String },
    /// The issue's IC is alive right now. Dispatching would put two
    /// processes on one worktree.
    SessionLive { session_name: String },
    /// The issue has a session, and the registry could not be read to say
    /// whether it is alive.
    RegistryUnreadable { detail: String },
    /// Eligible, but every slot is taken.
    ConcurrencyCap { in_flight: usize, cap: usize },
    /// The day's ledger cannot authorize another dispatch. Global: when this
    /// fires, every candidate carries it.
    DailyBudgetExhausted { spent_usd: f64, budget_usd: f64 },
    /// The last run refused because the rendered goal condition exceeds its
    /// limit even after reduction. That refusal repeats until a human
    /// shortens the issue or its gates, so it is not resumed ahead of the
    /// backlog every tick; `autopilot clear-dispatch` releases it.
    ConditionTooLong,
    /// The last run's harness reported `session_uuid` still in use after an
    /// invocation finished: something still holds it. Retrying can never
    /// succeed and a fresh session could start a second implementer, so it
    /// waits for an operator to find the holder and run
    /// `autopilot clear-dispatch`.
    SessionInUse { session_uuid: String },
    /// The issue's dispatch-state drawer cannot be read: it is corrupt, or a
    /// newer binary wrote it for a harness this one does not know. It may be
    /// a live invocation, so it holds its slot and its issue until a binary
    /// that reads it (or a human who has checked nothing runs for it)
    /// releases it.
    DispatchStateUnreadable,
    /// The issue's dispatch state was written by the other harness, which the
    /// configured runner refuses to resume. Only that harness, or a human
    /// clearing the state, moves it.
    HarnessMismatch {
        persisted: Implementer,
        configured: Implementer,
    },
}

/// One deferred issue and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Deferred {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub priority: Priority,
    pub reason: DeferReason,
}

/// One pass of the cross-repo queue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct QueuePlan {
    /// Selected, highest priority first. Resumes lead.
    pub dispatch: Vec<QueuedIssue>,
    /// Everything else seen this pass, with a reason each.
    pub deferred: Vec<Deferred>,
    /// In-flight issues that were **not** selected — the slots this pass
    /// could not use.
    pub occupied_slots: usize,
    pub concurrency_cap: usize,
    pub spent_today_usd: f64,
    pub daily_budget_usd: f64,
    /// Unpriced IC dispatches today (timed-out runs). Any unpriced
    /// counter non-zero means `spent_today_usd` is a floor.
    pub unpriced_dispatch_count: u32,
    /// Unpriced reviews today. See `unpriced_dispatch_count`.
    pub unpriced_review_count: u32,
    /// Unpriced advisor calls today. See `unpriced_dispatch_count`.
    pub unpriced_advice_count: u32,
}

impl QueuePlan {
    /// Whether the day's total is a floor, not a total: any unpriced
    /// counter (dispatch, review, or advice) is non-zero.
    pub fn has_unpriced_spend(&self) -> bool {
        self.unpriced_dispatch_count > 0
            || self.unpriced_review_count > 0
            || self.unpriced_advice_count > 0
    }

    pub fn is_empty(&self) -> bool {
        self.dispatch.is_empty()
    }
}

/// Choose what to work on next, across every repo.
///
/// Pure over `backlogs`, `snapshot` and the database — it writes nothing and
/// touches no network. See the module doc for the ordering rules and for what
/// an unreadable registry may and may not stop.
pub fn plan_queue(
    db: &Database,
    backlogs: &[RepoBacklog],
    snapshot: &dyn LivenessSnapshot,
    config: &QueueConfig,
) -> Result<QueuePlan, MemoryError> {
    config.validate()?;
    for backlog in backlogs {
        validate_repo(&backlog.repo)?;
    }

    // Every issue with a dispatch-state drawer, keyed for lookup. Counted
    // from drawers rather than from the registry on purpose: a slot is held
    // by *work in progress*, and an IC that has exited between dispatches has
    // not given its slot back.
    //
    // A drawer that cannot be read (corrupt, or written by a newer binary
    // for a harness this one does not know) holds its slot and its issue:
    // it may be a live invocation, and skipping it would dispatch past the
    // cap, or a second implementer onto its worktree.
    let dispatch_state::DispatchStateScan {
        states: in_flight,
        unreadable,
    } = dispatch_state::scan_dispatch_states(db, IN_FLIGHT_SCAN_LIMIT)?;

    // The same predicate `run_issue` uses, deliberately spelled the same way
    // rather than approximated. Note what this does *not* mirror: the
    // per-repo unpriced-dispatch ceiling rung 7 added. That one is conditional
    // on the repo having a wall-clock bound, and duplicating the condition
    // here to get it subtly wrong would be worse than letting `run_issue`
    // report it — which it does, terminally and without spending anything.
    let ledger_today = budget::get_daily_spend(db, &today_utc())?;
    let spent_today_usd = ledger_today
        .as_ref()
        .map(|entry| entry.total_cost_usd)
        .unwrap_or(0.0);
    let unpriced_dispatch_count = ledger_today
        .as_ref()
        .map(|e| e.unpriced_dispatch_count)
        .unwrap_or(0);
    let unpriced_review_count = ledger_today
        .as_ref()
        .map(|e| e.unpriced_review_count)
        .unwrap_or(0);
    let unpriced_advice_count = ledger_today
        .as_ref()
        .map(|e| e.unpriced_advice_count)
        .unwrap_or(0);
    let budget_exhausted = spent_today_usd + config.max_budget_usd > config.daily_budget_usd;

    let mut candidates: Vec<QueuedIssue> = Vec::new();
    let mut deferred: Vec<Deferred> = Vec::new();

    for backlog in backlogs {
        let repo_approved = gate_config::is_gate_config_approved(db, &backlog.repo)?;
        for listing in &backlog.issues {
            let issue = IssueRef::new(backlog.repo.clone(), listing.number);
            let priority = Priority::of(&listing.labels);
            let mut defer = |reason: DeferReason| {
                deferred.push(Deferred {
                    issue: issue.clone(),
                    priority,
                    reason,
                });
            };

            // Global before per-issue: when the day's ledger is spent the
            // Lead "stops dispatching and reports", and reporting eight
            // different per-issue reasons for one global stop would bury it.
            if budget_exhausted {
                defer(DeferReason::DailyBudgetExhausted {
                    spent_usd: spent_today_usd,
                    budget_usd: config.daily_budget_usd,
                });
                continue;
            }
            if !repo_approved {
                defer(DeferReason::RepoNotApproved);
                continue;
            }
            let eligibility = labels::eligibility(&listing.labels);
            if !eligibility.is_eligible() {
                defer(DeferReason::NotEligible { eligibility });
                continue;
            }

            let status = lineage::get_issue_status(db, &issue)?;
            // A recorded success ends the issue — unless rung 11 has re-opened
            // it. A reviewer's `needs_changes` verdict is the spec's own reason
            // to dispatch a succeeded issue again, and the cap check
            // immediately below is what keeps that bounded rather than
            // endless — it is deliberately *not* skipped for a remediation.
            // The remediation is charged against a cap of its own, counted
            // from the issue's first success (#344), not the remainder of the
            // issue's.
            //
            // The success is tested first so `&&` short-circuits: only a
            // succeeded issue can be deferred here, and reading the
            // remediation drawer unconditionally cost every in-progress issue
            // an extra drawer read plus a second read of the lineage status
            // this line already holds — on every issue, on every tick.
            let succeeded = status
                .as_ref()
                .is_some_and(|s| s.best_verdict == Some(AttemptOutcome::Success));
            if succeeded && remediate::active_remediation(db, &issue)?.is_none() {
                defer(DeferReason::AlreadySucceeded);
                continue;
            }
            let cumulative_attempt_n = status.as_ref().map(|s| s.cumulative_attempt_n).unwrap_or(0);
            // Read through the same count `run_issue` applies. The cap is
            // enforced in both places on purpose ("same predicate, one place
            // each"), so both must read the *charged* count: a queue that
            // compared the raw lifetime counter would defer a human-retried
            // issue — or a remediation of a success that spent the whole cap —
            // as `AttemptCapReached` on every tick and `run_issue` would never
            // see it.
            let charged = remediate::attempts_charged(db, &issue, cumulative_attempt_n, succeeded)?;
            if charged >= config.attempt_cap {
                defer(DeferReason::AttemptCapReached {
                    cumulative_attempt_n,
                });
                continue;
            }

            if let Some(signature) = supervise::escalated_signature(db, &issue)? {
                defer(DeferReason::StrategyEscalated { signature });
                continue;
            }

            if unreadable.contains(&dispatch_state::dispatch_state_key(&issue)) {
                defer(DeferReason::DispatchStateUnreadable);
                continue;
            }
            let state = in_flight.iter().find(|s| s.issue == issue);
            if let Some(state) = state {
                match snapshot.liveness(state) {
                    Liveness::Alive => {
                        defer(DeferReason::SessionLive {
                            session_name: state.ic_session_name.clone(),
                        });
                        continue;
                    }
                    Liveness::Unknown => {
                        defer(DeferReason::RegistryUnreadable {
                            detail: snapshot.state_reason(state),
                        });
                        continue;
                    }
                    Liveness::NotListed => {}
                }
                if state.state == "paused-condition-too-long" {
                    defer(DeferReason::ConditionTooLong);
                    continue;
                }
                if state.state == "paused-session-in-use" {
                    defer(DeferReason::SessionInUse {
                        session_uuid: state.session_uuid.clone(),
                    });
                    continue;
                }
                if let Some(configured) = config.implementer {
                    if state.implementer != configured {
                        defer(DeferReason::HarnessMismatch {
                            persisted: state.implementer,
                            configured,
                        });
                        continue;
                    }
                }
            }

            candidates.push(QueuedIssue {
                issue,
                title: listing.title.clone(),
                body: listing.body.clone(),
                priority,
                resuming: state.is_some(),
                cumulative_attempt_n,
                risk_label: risk_label(&listing.labels),
            });
        }
    }

    sort_candidates(&mut candidates);

    // Admit under the cap. The count that matters is how many issues are in
    // flight *after* this pass, so an admitted resume re-occupies the slot it
    // already held rather than consuming a second one.
    let mut selected: Vec<QueuedIssue> = Vec::new();
    for candidate in candidates {
        let already_held = candidate.resuming;
        let in_flight_after =
            in_flight.len() + unreadable.len() + selected.iter().filter(|s| !s.resuming).count();
        if !already_held && in_flight_after >= config.concurrency_cap {
            deferred.push(Deferred {
                issue: candidate.issue,
                priority: candidate.priority,
                reason: DeferReason::ConcurrencyCap {
                    in_flight: in_flight_after,
                    cap: config.concurrency_cap,
                },
            });
            continue;
        }
        selected.push(candidate);
    }

    let occupied_slots = in_flight
        .iter()
        .filter(|state| !selected.iter().any(|s| s.issue == state.issue))
        .count()
        + unreadable.len();

    Ok(QueuePlan {
        dispatch: selected,
        deferred,
        occupied_slots,
        concurrency_cap: config.concurrency_cap,
        spent_today_usd,
        daily_budget_usd: config.daily_budget_usd,
        unpriced_dispatch_count,
        unpriced_review_count,
        unpriced_advice_count,
    })
}

/// Order the queue.
///
/// Every key is immutable data, deliberately: `updatedAt` would be the
/// obvious fairness tiebreak, and it would make the plan irreproducible —
/// a comment on an issue would silently reorder the queue between two passes
/// that saw exactly the same work. Issue number ascending is the same
/// oldest-first intent expressed in a value that never changes.
fn sort_candidates(candidates: &mut [QueuedIssue]) {
    candidates.sort_by(|a, b| {
        b.resuming
            .cmp(&a.resuming)
            .then(b.priority.cmp(&a.priority))
            .then(a.issue.repo.cmp(&b.issue.repo))
            .then(a.issue.number.cmp(&b.issue.number))
    });
}

/// The value of an issue's `risk:<class>` label, lowercased, if it has one.
///
/// Case-insensitive for the reason [`Priority::of`] is: GitHub label names
/// are, so an exact-match read would treat `Risk:Logic` as a foreign label.
/// The value is **not** checked against [`super::review::RiskClass`] — one
/// place decides what a class means, and it is
/// [`super::review::decide_merge`], where an unrecognized class holds the PR
/// for a human instead of merging it.
///
/// # Conflicting labels resolve the opposite way to [`Priority::of`]
///
/// Priority can afford "highest wins" because ordering is not a safety
/// boundary (see the module doc). The risk class *is* one: it is the value
/// rung 5's `decide_merge` compares the Reviewer's classification against,
/// and it is what lets a PR auto-merge without a human. An issue carrying
/// both `risk:documentation` and `risk:logic` has not been judged to be
/// either, and picking whichever GitHub happened to list first would let a
/// logic change ride in on the documentation rule.
///
/// So distinct values are joined instead: `documentation+logic` is not a
/// [`super::review::RiskClass`], so it lands at the same `ClassMismatch`
/// hold an unrecognized class does — fail-closed, and self-explaining in the
/// dispatch-state drawer. Sorted so the value does not depend on label order.
pub(super) fn risk_label(labels: &[String]) -> Option<String> {
    let mut found: Vec<String> = Vec::new();
    for label in labels {
        let normalized = label.trim().to_ascii_lowercase();
        let Some(rest) = normalized.strip_prefix(super::lead::RISK_LABEL_PREFIX) else {
            continue;
        };
        if rest.is_empty() || found.iter().any(|f| f == rest) {
            continue;
        }
        found.push(rest.to_string());
    }
    found.sort();
    match found.len() {
        0 => None,
        1 => found.pop(),
        _ => Some(found.join("+")),
    }
}

/// How many dispatch-state drawers one queue pass will read.
///
/// Enumerated by logical-key prefix in SQL (rung 7's
/// `get_drawers_by_source_prefix`), so this bounds a genuinely in-flight set
/// rather than competing with lineage traffic for a room-wide window. Far
/// above any plausible concurrency cap: undercounting in-flight work is the
/// one error here that *over*-dispatches.
pub(crate) const IN_FLIGHT_SCAN_LIMIT: usize = 1_000;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::registry::AgentEntry;
    use crate::autopilot::{lineage, retry, DispatchState};

    const REPO: &str = "ironrace/ironmem";
    const OTHER: &str = "ironrace/other";

    fn listing(number: u64, labels: &[&str]) -> IssueListing {
        IssueListing {
            number,
            title: format!("issue {number}"),
            body: "body".to_string(),
            labels: labels.iter().map(|s| s.to_string()).collect(),
            updated_at: "2026-09-02T00:00:00Z".to_string(),
        }
    }

    fn ready(number: u64) -> IssueListing {
        listing(number, &["agent:ready"])
    }

    fn backlog(repo: &str, issues: Vec<IssueListing>) -> RepoBacklog {
        RepoBacklog {
            repo: repo.to_string(),
            issues,
        }
    }

    fn empty_registry() -> RegistrySnapshot {
        RegistrySnapshot::Available(Vec::new())
    }

    fn approved(repos: &[&str]) -> Database {
        let db = Database::open_in_memory().unwrap();
        for repo in repos {
            gate_config::propose_gate_config(
                &db,
                repo,
                vec!["cargo test --workspace".to_string()],
                Vec::new(),
            )
            .unwrap();
            gate_config::approve_gate_config(&db, repo).unwrap();
        }
        db
    }

    fn config() -> QueueConfig {
        QueueConfig::default()
    }

    fn put_state(db: &Database, issue: &IssueRef, session_name: &str) {
        put_state_for(
            db,
            issue,
            session_name,
            crate::autopilot::implementer::Implementer::Claude,
        );
    }

    fn put_state_for(
        db: &Database,
        issue: &IssueRef,
        session_name: &str,
        implementer: crate::autopilot::implementer::Implementer,
    ) {
        dispatch_state::upsert_dispatch_state(
            db,
            &DispatchState {
                implementer,
                native_session_id: None,
                invocation_artifacts: None,
                issue: issue.clone(),
                worktree_path: "/tmp/wt".to_string(),
                ic_session_name: session_name.to_string(),
                dispatch_class: "logic".to_string(),
                attempt_n: 1,
                state: "dispatching".to_string(),
                started_at: "2026-09-02T00:00:00Z".to_string(),
                session_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                turn_n: 1,
                session_claimed: true,
                session_model: None,
                resume_refusals: 0,
                signing_held: false,
            },
        )
        .unwrap();
    }

    fn put_status(db: &Database, issue: &IssueRef, attempts: u32, succeeded: bool) {
        lineage::upsert_issue_status(
            db,
            &lineage::IssueStatus {
                issue: issue.clone(),
                cumulative_attempt_n: attempts,
                best_verdict: succeeded.then_some(AttemptOutcome::Success),
                best_commit_sha: None,
            },
        )
        .unwrap();
    }

    /// Record the issue's original work going green on attempt `attempts`,
    /// in the lineage (which the remediation budget is counted from) and in
    /// its status.
    fn succeed_at(db: &Database, issue: &IssueRef, attempts: u32, commit: Option<&str>) {
        lineage::record_attempt(
            db,
            &lineage::AttemptRecord {
                issue: issue.clone(),
                attempt_n: attempts,
                approach: "went green".to_string(),
                verdict: AttemptOutcome::Success,
                why_failed: None,
                commit_sha: commit.map(str::to_string),
            },
        )
        .unwrap();
        lineage::upsert_issue_status(
            db,
            &lineage::IssueStatus {
                issue: issue.clone(),
                cumulative_attempt_n: attempts,
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: commit.map(str::to_string),
            },
        )
        .unwrap();
    }

    /// Arm a remediation on an issue whose success is recorded at `commit`,
    /// on attempt 1.
    fn arm(db: &Database, issue: &IssueRef, commit: Option<&str>, cap: u32) {
        succeed_at(db, issue, 1, commit);
        let outcome = remediate::arm_remediation(
            db,
            &remediate::ArmRequest {
                issue,
                pr_number: 7,
                head_sha: commit.unwrap_or("abc123"),
                findings: Some("the retry loop is unbounded"),
                attempt_cap: cap,
            },
        )
        .unwrap();
        assert!(outcome.in_force(), "test setup did not arm: {outcome:?}");
    }

    #[test]
    fn plan_shows_both_unpriced_counts_and_marks_the_total_a_floor() {
        let db = approved(&[REPO]);
        budget::record_unpriced_dispatch(&db, &today_utc()).unwrap();
        budget::record_unpriced_review(&db, &today_utc()).unwrap();
        budget::record_unpriced_review(&db, &today_utc()).unwrap();
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert_eq!(plan.unpriced_dispatch_count, 1);
        assert_eq!(plan.unpriced_review_count, 2);
        assert!(plan.has_unpriced_spend());
    }

    #[test]
    fn floor_predicate_fires_on_a_single_unpriced_counter() {
        // Mutation guard for `||`, including the advice arm.
        let db = approved(&[REPO]);
        budget::record_unpriced_review(&db, &today_utc()).unwrap();
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert_eq!(plan.unpriced_dispatch_count, 0);
        assert_eq!(plan.unpriced_review_count, 1);
        assert!(plan.has_unpriced_spend(), "review alone is a floor");

        let db = approved(&[REPO]);
        budget::record_advice_call(&db, &today_utc(), None).unwrap();
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert_eq!(plan.unpriced_advice_count, 1);
        assert!(plan.has_unpriced_spend(), "advice alone is a floor");
    }

    #[test]
    fn a_succeeded_issue_with_a_remediation_in_force_is_dispatched() {
        // The spec's red path: "re-dispatch the IC to fix, counting against
        // the same per-issue attempt cap". Without this the reviewer's
        // findings reach nobody and rung 11 is a drawer nothing reads.
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        arm(&db, &issue, Some("abc123"), 5);

        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert_eq!(
            numbers(&plan),
            vec![1],
            "a succeeded issue a reviewer objected to is dispatchable again"
        );
    }

    #[test]
    fn a_succeeded_issue_is_still_deferred_when_no_remediation_is_armed() {
        // Rung 10's behaviour, unchanged. The new exit is narrow on purpose.
        let db = approved(&[REPO]);
        put_status(&db, &IssueRef::new(REPO, 1), 1, true);
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert_eq!(reason_for(&plan, 1), DeferReason::AlreadySucceeded);
    }

    #[test]
    fn a_remediation_does_not_exempt_an_issue_from_the_attempt_cap() {
        // The bound the spec names. A remediation that skipped the cap would
        // re-dispatch a stuck issue forever at $2.50 an attempt, and the
        // "on exhaustion the PR stays open for a human" half would never
        // arrive.
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        let mut cfg = config();
        cfg.attempt_cap = 5;
        arm(&db, &issue, Some("abc123"), 5);
        // Four remediation attempts since the success at attempt 1: one left.
        put_status(&db, &issue, 5, true);
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(numbers(&plan), vec![1], "four of five: still selectable");

        // The fifth spends the remediation's own cap.
        put_status(&db, &issue, 6, true);

        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::AttemptCapReached {
                cumulative_attempt_n: 6
            }
        );
    }

    #[test]
    fn a_success_on_the_last_attempt_is_still_selected_for_its_remediation() {
        // Issue #344: the remediation's budget is its own, counted from the
        // issue's first success. A success at attempt 5 of 5 used to leave
        // nothing: `arm_remediation` refused, and an issue whose cap was
        // spent was never selected again.
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        let mut cfg = config();
        cfg.attempt_cap = 5;
        succeed_at(&db, &issue, 5, None);
        let outcome = remediate::arm_remediation(
            &db,
            &remediate::ArmRequest {
                issue: &issue,
                pr_number: 7,
                head_sha: "abc123",
                findings: Some("the retry loop is unbounded"),
                attempt_cap: 5,
            },
        )
        .unwrap();
        assert!(outcome.in_force(), "setup did not arm: {outcome:?}");

        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(numbers(&plan), vec![1]);
    }

    #[test]
    fn a_human_retry_makes_a_capped_issue_selectable_again() {
        // The other half of `autopilot retry`. `run_issue` reads the grant,
        // but nothing reaches `run_issue` that the queue did not select
        // first — so a queue comparing the raw lifetime counter would defer
        // the retried issue as `AttemptCapReached` on every tick and the
        // recovery would still never happen.
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        let mut cfg = config();
        cfg.attempt_cap = 5;
        put_status(&db, &issue, 5, false);

        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::AttemptCapReached {
                cumulative_attempt_n: 5
            },
            "setup: the issue must be capped before the retry"
        );

        retry::grant_retry(&db, &issue, 5).unwrap();
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(numbers(&plan), vec![1], "a forgiven issue is selectable");
    }

    #[test]
    fn a_remediation_superseded_by_a_newer_success_stops_dispatching() {
        // The IC pushed the fix and it went green: rung 10 reviews the new
        // head, and the Lead must stop re-dispatching against findings that
        // have already been addressed.
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        arm(&db, &issue, Some("abc123"), 5);
        put_status(&db, &issue, 2, true);
        lineage::upsert_issue_status(
            &db,
            &lineage::IssueStatus {
                issue: issue.clone(),
                cumulative_attempt_n: 2,
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: Some("def456".to_string()),
            },
        )
        .unwrap();

        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert_eq!(reason_for(&plan, 1), DeferReason::AlreadySucceeded);
    }

    #[test]
    fn a_remediation_does_not_override_a_human_who_blocked_the_issue() {
        // Label eligibility is checked before the success test and stays
        // there: `agent:blocked` is a human taking the issue back, and a
        // reviewer's objection is not authority to overrule that.
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        arm(&db, &issue, Some("abc123"), 5);

        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![listing(1, &["agent:blocked"])])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert!(
            matches!(reason_for(&plan, 1), DeferReason::NotEligible { .. }),
            "a human's block outranks a reviewer's objection"
        );
    }

    fn numbers(plan: &QueuePlan) -> Vec<u64> {
        plan.dispatch.iter().map(|q| q.issue.number).collect()
    }

    fn reason_for(plan: &QueuePlan, number: u64) -> DeferReason {
        plan.deferred
            .iter()
            .find(|d| d.issue.number == number)
            .unwrap_or_else(|| panic!("issue {number} was not deferred"))
            .reason
            .clone()
    }

    // ── priority ────────────────────────────────────────────────────────

    #[test]
    fn the_priority_vocabulary_is_the_one_this_repo_actually_uses() {
        // Measured with `gh label list` (free, read-only): exactly three
        // labels, and no `priority:critical`.
        assert_eq!(Priority::of(&["priority:high".into()]), Priority::High);
        assert_eq!(Priority::of(&["priority:medium".into()]), Priority::Medium);
        assert_eq!(Priority::of(&["priority:low".into()]), Priority::Low);
        assert_eq!(Priority::of(&["bug".into()]), Priority::Unlabeled);
        assert_eq!(
            Priority::of(&["priority:critical".into()]),
            Priority::Unlabeled
        );
    }

    #[test]
    fn an_untriaged_issue_outranks_one_explicitly_marked_optional() {
        assert!(Priority::Unlabeled > Priority::Low);
        assert!(Priority::Medium > Priority::Unlabeled);
    }

    #[test]
    fn conflicting_priority_labels_resolve_to_the_highest() {
        assert_eq!(
            Priority::of(&["priority:low".into(), "priority:high".into()]),
            Priority::High
        );
    }

    #[test]
    fn priority_labels_are_read_case_insensitively() {
        // GitHub label names are case-insensitive; rung 6 learned this when
        // an exact-match parse made a stop sign invisible.
        assert_eq!(Priority::of(&["Priority:High".into()]), Priority::High);
        assert_eq!(Priority::of(&[" priority:HIGH ".into()]), Priority::High);
    }

    #[test]
    fn a_risk_label_is_read_verbatim_and_not_validated_here() {
        assert_eq!(
            risk_label(&["risk:logic".into()]),
            Some("logic".to_string())
        );
        assert_eq!(
            risk_label(&["Risk:Logic".into()]),
            Some("logic".to_string())
        );
        // Not a RiskClass. Passed through, so `decide_merge` holds it.
        assert_eq!(
            risk_label(&["risk:banana".into()]),
            Some("banana".to_string())
        );
        assert_eq!(risk_label(&["risk:".into()]), None);
        assert_eq!(risk_label(&["bug".into()]), None);
    }

    #[test]
    fn conflicting_risk_labels_fail_closed_rather_than_picking_one() {
        // Unlike `priority:*`, the risk class is a safety boundary: it is
        // what `decide_merge` compares the Reviewer against, and picking
        // whichever GitHub listed first would let a logic change ride in on
        // the documentation rule. The joined value parses as no `RiskClass`,
        // so it holds at `ClassMismatch`.
        let combined = risk_label(&["risk:logic".into(), "risk:documentation".into()]).unwrap();
        assert_eq!(combined, "documentation+logic");
        assert!(super::super::review::RiskClass::from_schema_str(&combined).is_none());
        // Order-independent: the same two labels always produce the same
        // value, so two passes over one issue cannot disagree.
        assert_eq!(
            risk_label(&["risk:documentation".into(), "risk:logic".into()]),
            Some(combined)
        );
        // The same label twice is not a conflict.
        assert_eq!(
            risk_label(&["risk:logic".into(), "Risk:Logic".into()]),
            Some("logic".to_string())
        );
    }

    // ── ordering ────────────────────────────────────────────────────────

    #[test]
    fn the_queue_is_ordered_by_priority_then_oldest_issue_first() {
        let db = approved(&[REPO]);
        let mut cfg = config();
        cfg.concurrency_cap = 10;
        let plan = plan_queue(
            &db,
            &[backlog(
                REPO,
                vec![
                    listing(9, &["agent:ready", "priority:low"]),
                    listing(3, &["agent:ready"]),
                    listing(7, &["agent:ready", "priority:high"]),
                    listing(1, &["agent:ready", "priority:high"]),
                    listing(5, &["agent:ready", "priority:medium"]),
                ],
            )],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(numbers(&plan), vec![1, 7, 5, 3, 9]);
    }

    #[test]
    fn the_order_does_not_depend_on_when_an_issue_was_last_touched() {
        // `updatedAt` would make the plan irreproducible: a comment would
        // reorder the queue between two passes that saw the same work.
        let db = approved(&[REPO]);
        let mut cfg = config();
        cfg.concurrency_cap = 10;
        let mut old_but_recent = ready(1);
        old_but_recent.updated_at = "2026-09-02T23:59:59Z".to_string();
        let mut new_but_stale = ready(2);
        new_but_stale.updated_at = "2020-01-01T00:00:00Z".to_string();
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![new_but_stale, old_but_recent])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(numbers(&plan), vec![1, 2]);
    }

    #[test]
    fn repos_are_ordered_deterministically_at_equal_priority() {
        let db = approved(&[REPO, OTHER]);
        let mut cfg = config();
        cfg.concurrency_cap = 10;
        let plan = plan_queue(
            &db,
            &[
                backlog(REPO, vec![ready(5)]),
                backlog(OTHER, vec![ready(5)]),
            ],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(plan.dispatch[0].issue.repo, REPO);
        assert_eq!(plan.dispatch[1].issue.repo, OTHER);
    }

    #[test]
    fn an_in_flight_issue_is_worked_before_any_new_one_whatever_its_priority() {
        // Resuming costs ~5% of a cold start (rung 0), and leaving sessions
        // parked while starting fresh work is how half-finished issues pile
        // up.
        let db = approved(&[REPO]);
        let mut cfg = config();
        cfg.concurrency_cap = 10;
        put_state(&db, &IssueRef::new(REPO, 9), "ic-ironrace-ironmem-9");
        let plan = plan_queue(
            &db,
            &[backlog(
                REPO,
                vec![
                    listing(1, &["agent:ready", "priority:high"]),
                    listing(9, &["agent:ready", "priority:low"]),
                ],
            )],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(numbers(&plan), vec![9, 1]);
        assert!(plan.dispatch[0].resuming);
        assert!(!plan.dispatch[1].resuming);
    }

    // ── guards ──────────────────────────────────────────────────────────

    #[test]
    fn an_unapproved_repo_dispatches_nothing() {
        let db = Database::open_in_memory().unwrap();
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert!(plan.is_empty());
        assert_eq!(reason_for(&plan, 1), DeferReason::RepoNotApproved);
    }

    #[test]
    fn a_stop_label_beats_the_ready_label_that_put_the_issue_in_the_listing() {
        // `gh issue list --label agent:ready` returns issues that also carry
        // `agent:exhausted`, because a human can add one without removing
        // the other. Rung 6's eligibility resolves toward the more
        // restrictive state; this is where that matters.
        let db = approved(&[REPO]);
        let plan = plan_queue(
            &db,
            &[backlog(
                REPO,
                vec![listing(1, &["agent:ready", "agent:exhausted"])],
            )],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert!(plan.is_empty());
        assert!(matches!(
            reason_for(&plan, 1),
            DeferReason::NotEligible { .. }
        ));
    }

    #[test]
    fn an_issue_that_already_succeeded_is_not_re_dispatched() {
        let db = approved(&[REPO]);
        put_status(&db, &IssueRef::new(REPO, 1), 2, true);
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert_eq!(reason_for(&plan, 1), DeferReason::AlreadySucceeded);
    }

    #[test]
    fn an_issue_at_its_attempt_cap_is_deferred_rather_than_dispatched() {
        let db = approved(&[REPO]);
        let mut cfg = config();
        cfg.attempt_cap = 5;
        put_status(&db, &IssueRef::new(REPO, 1), 5, false);
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::AttemptCapReached {
                cumulative_attempt_n: 5
            }
        );
    }

    #[test]
    fn an_escalated_issue_is_not_picked_until_a_human_clears_it() {
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        supervise::upsert_supervision(
            &db,
            &supervise::SupervisionRecord {
                issue: issue.clone(),
                fingerprint: "f".to_string(),
                progress_observed_at: "2026-09-02T00:00:00Z".to_string(),
                first_absent_at: None,
                last_checked_at: "2026-09-02T00:00:00Z".to_string(),
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
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::StrategyEscalated {
                signature: "the same failure".to_string()
            }
        );
    }

    #[test]
    fn an_issue_whose_ic_is_alive_right_now_is_never_dispatched_again() {
        // Two dispatches on one worktree is the collision rung 7 refused to
        // risk when it declined to restart a listed-but-stalled session.
        let db = approved(&[REPO]);
        put_state(&db, &IssueRef::new(REPO, 1), "ic-ironrace-ironmem-1");
        let snapshot = RegistrySnapshot::Available(vec![AgentEntry {
            name: "ic-ironrace-ironmem-1".to_string(),
            status: None,
        }]);
        let plan = plan_queue(&db, &[backlog(REPO, vec![ready(1)])], &snapshot, &config()).unwrap();
        assert!(plan.is_empty());
        assert!(matches!(
            reason_for(&plan, 1),
            DeferReason::SessionLive { .. }
        ));
    }

    #[test]
    fn an_unreadable_registry_holds_resumes_but_not_fresh_work() {
        // Rung 7's lesson 21, scoped to what the registry is actually being
        // asked. An issue with no session has nothing to collide with.
        let db = approved(&[REPO]);
        put_state(&db, &IssueRef::new(REPO, 1), "ic-ironrace-ironmem-1");
        let snapshot = RegistrySnapshot::Unavailable {
            reason: "claude agents --json exited 1".to_string(),
        };
        let mut cfg = config();
        cfg.concurrency_cap = 10;
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1), ready(2)])],
            &snapshot,
            &cfg,
        )
        .unwrap();
        assert_eq!(numbers(&plan), vec![2]);
        assert!(matches!(
            reason_for(&plan, 1),
            DeferReason::RegistryUnreadable { .. }
        ));
    }

    // ── the two caps ────────────────────────────────────────────────────

    #[test]
    fn the_concurrency_cap_counts_drawers_not_live_processes() {
        // An IC exits at the end of every dispatch by design. If the cap
        // counted processes, every gap between dispatches would look like a
        // free slot.
        let db = approved(&[REPO]);
        let mut cfg = config();
        cfg.concurrency_cap = 2;
        put_state(&db, &IssueRef::new(REPO, 8), "ic-ironrace-ironmem-8");
        put_state(&db, &IssueRef::new(REPO, 9), "ic-ironrace-ironmem-9");
        // Neither session is listed — both ICs have exited between
        // dispatches — and the registry read is clean.
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert!(plan.is_empty());
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::ConcurrencyCap {
                in_flight: 2,
                cap: 2
            }
        );
        assert_eq!(plan.occupied_slots, 2);
    }

    #[test]
    fn resuming_an_in_flight_issue_reuses_its_slot_rather_than_taking_a_second() {
        let db = approved(&[REPO]);
        let mut cfg = config();
        cfg.concurrency_cap = 1;
        put_state(&db, &IssueRef::new(REPO, 9), "ic-ironrace-ironmem-9");
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(9), ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(numbers(&plan), vec![9]);
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::ConcurrencyCap {
                in_flight: 1,
                cap: 1
            }
        );
        assert_eq!(
            plan.occupied_slots, 0,
            "the resumed slot is in use, not idle"
        );
    }

    #[test]
    fn the_budget_predicate_is_the_one_run_issue_will_apply() {
        // `run_issue` pre-authorizes: `spent + max_budget_usd >
        // daily_budget_usd` stops it. A queue that admitted on `spent >=
        // daily` would report a dispatch that never happens.
        let db = approved(&[REPO]);
        let mut cfg = config();
        cfg.daily_budget_usd = 10.0;
        cfg.max_budget_usd = 2.5;
        budget::accumulate_daily_spend(&db, &today_utc(), 8.0).unwrap();
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert!(
            plan.is_empty(),
            "$8.00 spent + a $2.50 ceiling exceeds a $10.00 day"
        );
        assert!(matches!(
            reason_for(&plan, 1),
            DeferReason::DailyBudgetExhausted { .. }
        ));
    }

    #[test]
    fn a_spent_day_reports_the_budget_reason_for_every_issue_not_eight_different_ones() {
        let db = approved(&[REPO]);
        let mut cfg = config();
        cfg.daily_budget_usd = 1.0;
        cfg.max_budget_usd = 1.0;
        budget::accumulate_daily_spend(&db, &today_utc(), 5.0).unwrap();
        put_status(&db, &IssueRef::new(REPO, 2), 99, true);
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1), ready(2)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert!(plan.is_empty());
        for number in [1, 2] {
            assert!(matches!(
                reason_for(&plan, number),
                DeferReason::DailyBudgetExhausted { .. }
            ));
        }
    }

    // ── configuration ───────────────────────────────────────────────────

    #[test]
    fn a_nan_daily_budget_is_refused_rather_than_silently_removing_the_ceiling() {
        // `spent >= NaN` is false forever. Rung 5's finding #3, inherited
        // rather than re-learned (rung 7's lesson 26).
        let mut cfg = config();
        cfg.daily_budget_usd = f64::NAN;
        assert!(cfg.validate().is_err());
        cfg.daily_budget_usd = f64::INFINITY;
        assert!(cfg.validate().is_err());
        cfg.daily_budget_usd = 0.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn a_per_dispatch_ceiling_above_the_daily_one_is_refused() {
        let mut cfg = config();
        cfg.daily_budget_usd = 1.0;
        cfg.max_budget_usd = 2.0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn a_zero_concurrency_cap_is_refused() {
        let mut cfg = config();
        cfg.concurrency_cap = 0;
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn an_empty_backlog_set_plans_nothing_without_erroring() {
        let db = approved(&[REPO]);
        let plan = plan_queue(&db, &[], &empty_registry(), &config()).unwrap();
        assert!(plan.is_empty());
        assert!(plan.deferred.is_empty());
    }

    #[test]
    fn planning_writes_nothing() {
        let db = approved(&[REPO]);
        plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1)])],
            &empty_registry(),
            &config(),
        )
        .unwrap();
        assert!(
            dispatch_state::get_dispatch_state(&db, &IssueRef::new(REPO, 1))
                .unwrap()
                .is_none(),
            "planning must not create state for an issue it merely considered"
        );
        assert!(lineage::get_issue_status(&db, &IssueRef::new(REPO, 1))
            .unwrap()
            .is_none());
    }
    #[test]
    fn codex_queue_routes_by_persisted_harness_and_holds_uncertainty() {
        use crate::autopilot::process_tree::{testing, ProcessSnapshot};
        use crate::autopilot::registry::HarnessSnapshots;
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        put_state(&db, &issue, "ic-test");
        let mut state = dispatch_state::get_dispatch_state(&db, &issue)
            .unwrap()
            .unwrap();
        state.implementer = crate::autopilot::implementer::Implementer::Codex;
        let dir = tempfile::tempdir().unwrap();
        let (a, row) = testing::invocation(dir.path());
        state.invocation_artifacts = Some(a.clone());
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let mut snapshots = HarnessSnapshots {
            claude: RegistrySnapshot::Available(Vec::new()),
            processes: ProcessSnapshot::Available(vec![row]),
        };
        let backlogs = [backlog(REPO, vec![ready(1)])];
        let plan = plan_queue(&db, &backlogs, &snapshots, &config()).unwrap();
        assert!(matches!(
            reason_for(&plan, 1),
            DeferReason::SessionLive { .. }
        ));
        snapshots.processes = ProcessSnapshot::Available(Vec::new());
        let plan = plan_queue(&db, &backlogs, &snapshots, &config()).unwrap();
        assert!(matches!(
            reason_for(&plan, 1),
            DeferReason::RegistryUnreadable { .. }
        ));
        testing::complete(&a);
        snapshots.claude = RegistrySnapshot::Available(vec![AgentEntry {
            name: "ic-test".into(),
            status: None,
        }]);
        let plan = plan_queue(&db, &backlogs, &snapshots, &config()).unwrap();
        assert_eq!(
            plan.dispatch.len(),
            1,
            "Claude name must not hold a dead Codex invocation"
        );
        assert_eq!(plan.dispatch[0].issue, issue);
    }
    #[test]
    fn a_condition_too_long_refusal_is_deferred_not_resumed_ahead_of_the_backlog() {
        use crate::autopilot::implementer::Implementer;
        use crate::autopilot::process_tree::ProcessSnapshot;
        use crate::autopilot::registry::HarnessSnapshots;
        let snapshots = HarnessSnapshots {
            claude: RegistrySnapshot::Available(Vec::new()),
            processes: ProcessSnapshot::Available(Vec::new()),
        };
        for implementer in [Implementer::Claude, Implementer::Codex] {
            let db = approved(&[REPO]);
            let issue = IssueRef::new(REPO, 1);
            put_state(&db, &issue, "ic-test");
            let mut state = dispatch_state::get_dispatch_state(&db, &issue)
                .unwrap()
                .unwrap();
            state.implementer = implementer;
            state.state = "paused-condition-too-long".into();
            dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
            // Idle, so liveness alone would resume it first — into the same
            // refusal, taking the tick's dispatch every tick.
            assert_eq!(snapshots.liveness(&state), Liveness::NotListed);
            let backlogs = [backlog(REPO, vec![ready(1), ready(2)])];
            let cfg = QueueConfig {
                implementer: Some(implementer),
                ..config()
            };
            let plan = plan_queue(&db, &backlogs, &snapshots, &cfg).unwrap();
            assert_eq!(reason_for(&plan, 1), DeferReason::ConditionTooLong);
            assert_eq!(plan.dispatch.len(), 1, "{implementer:?}");
            assert_eq!(plan.dispatch[0].issue, IssueRef::new(REPO, 2));
        }
    }
    /// A run that Muse refused because something still holds its session is
    /// parked: retrying can never succeed, so a Lead never re-dispatches it
    /// until an operator clears it.
    #[test]
    fn a_session_in_use_refusal_is_deferred_until_an_operator_clears_it() {
        use crate::autopilot::implementer::Implementer;
        use crate::autopilot::process_tree::ProcessSnapshot;
        use crate::autopilot::registry::HarnessSnapshots;
        let snapshots = HarnessSnapshots {
            claude: RegistrySnapshot::Available(Vec::new()),
            processes: ProcessSnapshot::Available(Vec::new()),
        };
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        put_state(&db, &issue, "ic-test");
        let mut state = dispatch_state::get_dispatch_state(&db, &issue)
            .unwrap()
            .unwrap();
        state.implementer = Implementer::Muse;
        state.session_uuid = "0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e".into();
        state.native_session_id = Some(state.session_uuid.clone());
        state.state = "paused-session-in-use".into();
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        assert_eq!(snapshots.liveness(&state), Liveness::NotListed);
        let backlogs = [backlog(REPO, vec![ready(1), ready(2)])];
        let cfg = QueueConfig {
            implementer: Some(Implementer::Muse),
            ..config()
        };
        let plan = plan_queue(&db, &backlogs, &snapshots, &cfg).unwrap();
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::SessionInUse {
                session_uuid: "0d6c1e2a-5b8f-4c1d-9e3a-7f2b6a1c4d5e".into()
            }
        );
        assert_eq!(plan.dispatch.len(), 1);
        assert_eq!(plan.dispatch[0].issue, IssueRef::new(REPO, 2));
    }
    #[test]
    fn a_state_owned_by_the_other_harness_is_deferred_not_dispatched_ahead_of_the_backlog() {
        use crate::autopilot::implementer::Implementer;
        let db = approved(&[REPO]);
        // A Claude Lead left issue 1 paused; its registry no longer lists the
        // session, so liveness alone would admit it as a resume and sort it
        // first — into a dispatch the configured runner must refuse every
        // tick.
        put_state(&db, &IssueRef::new(REPO, 1), "ic-stale");
        let backlogs = [backlog(REPO, vec![ready(1), ready(2)])];
        let snapshot = RegistrySnapshot::Available(Vec::new());
        let codex = QueueConfig {
            implementer: Some(Implementer::Codex),
            ..config()
        };
        let plan = plan_queue(&db, &backlogs, &snapshot, &codex).unwrap();
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::HarnessMismatch {
                persisted: Implementer::Claude,
                configured: Implementer::Codex,
            }
        );
        assert_eq!(plan.dispatch.len(), 1);
        assert_eq!(plan.dispatch[0].issue, IssueRef::new(REPO, 2));

        // The same harness resumes it, and an unscoped preview filters nothing.
        for scope in [Some(Implementer::Claude), None] {
            let cfg = QueueConfig {
                implementer: scope,
                ..config()
            };
            let plan = plan_queue(&db, &backlogs, &snapshot, &cfg).unwrap();
            assert_eq!(plan.dispatch[0].issue, IssueRef::new(REPO, 1));
        }
    }
    #[test]
    fn a_muse_state_is_deferred_for_a_codex_runner_not_dispatched_ahead_of_the_backlog() {
        use crate::autopilot::implementer::Implementer;
        use crate::autopilot::process_tree::ProcessSnapshot;
        use crate::autopilot::registry::HarnessSnapshots;
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        put_state_for(&db, &issue, "ic-stale", Implementer::Muse);
        // An idle pause that readable process evidence proves is not running.
        let mut state = dispatch_state::get_dispatch_state(&db, &issue)
            .unwrap()
            .unwrap();
        state.state = "paused-daily-budget".into();
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        let backlogs = [backlog(REPO, vec![ready(1), ready(2)])];
        // Claude's registry is unreadable, so a Muse state routed like Claude
        // would be held rather than deferred as a harness mismatch.
        let snapshot = HarnessSnapshots {
            claude: RegistrySnapshot::Unavailable {
                reason: "claude agents --json exited 1".to_string(),
            },
            processes: ProcessSnapshot::Available(Vec::new()),
        };
        let codex = QueueConfig {
            implementer: Some(Implementer::Codex),
            ..config()
        };
        let plan = plan_queue(&db, &backlogs, &snapshot, &codex).unwrap();
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::HarnessMismatch {
                persisted: Implementer::Muse,
                configured: Implementer::Codex,
            }
        );
        assert_eq!(plan.dispatch.len(), 1);
        assert_eq!(plan.dispatch[0].issue, IssueRef::new(REPO, 2));
    }
    #[test]
    fn muse_queue_routes_by_persisted_harness_and_holds_uncertainty() {
        use crate::autopilot::implementer::Implementer;
        use crate::autopilot::process_tree::{testing, ProcessSnapshot};
        use crate::autopilot::registry::HarnessSnapshots;
        let db = approved(&[REPO]);
        let issue = IssueRef::new(REPO, 1);
        put_state_for(&db, &issue, "ic-test", Implementer::Muse);
        let mut state = dispatch_state::get_dispatch_state(&db, &issue)
            .unwrap()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (a, row) = testing::invocation_for(dir.path(), Implementer::Muse);
        state.invocation_artifacts = Some(a.clone());
        dispatch_state::upsert_dispatch_state(&db, &state).unwrap();
        // Claude's registry lists nothing: only process evidence may hold it.
        let mut snapshots = HarnessSnapshots {
            claude: RegistrySnapshot::Available(Vec::new()),
            processes: ProcessSnapshot::Available(vec![row]),
        };
        let backlogs = [backlog(REPO, vec![ready(1), ready(2)])];
        let muse = QueueConfig {
            implementer: Some(Implementer::Muse),
            ..config()
        };
        // A live Muse invocation holds its issue: never a second implementer.
        let plan = plan_queue(&db, &backlogs, &snapshots, &muse).unwrap();
        assert!(
            matches!(reason_for(&plan, 1), DeferReason::SessionLive { .. }),
            "{:?}",
            reason_for(&plan, 1)
        );
        // Unreadable process evidence holds it too.
        snapshots.processes = ProcessSnapshot::Unavailable {
            reason: "ps exited 1".into(),
        };
        let plan = plan_queue(&db, &backlogs, &snapshots, &muse).unwrap();
        assert!(
            matches!(reason_for(&plan, 1), DeferReason::RegistryUnreadable { .. }),
            "{:?}",
            reason_for(&plan, 1)
        );
        // Gone, without a completion record: still held as ambiguous.
        snapshots.processes = ProcessSnapshot::Available(Vec::new());
        let plan = plan_queue(&db, &backlogs, &snapshots, &muse).unwrap();
        assert!(
            matches!(reason_for(&plan, 1), DeferReason::RegistryUnreadable { .. }),
            "{:?}",
            reason_for(&plan, 1)
        );
        // Completed: a Claude runner is refused it as the other harness's,
        // even with its session name listed live in Claude's registry.
        testing::complete(&a);
        snapshots.claude = RegistrySnapshot::Available(vec![AgentEntry {
            name: "ic-test".into(),
            status: None,
        }]);
        let claude = QueueConfig {
            implementer: Some(Implementer::Claude),
            ..config()
        };
        let plan = plan_queue(&db, &backlogs, &snapshots, &claude).unwrap();
        assert_eq!(
            reason_for(&plan, 1),
            DeferReason::HarnessMismatch {
                persisted: Implementer::Muse,
                configured: Implementer::Claude,
            }
        );
        assert_eq!(plan.dispatch[0].issue, IssueRef::new(REPO, 2));
        // And the Muse runner resumes it.
        let plan = plan_queue(&db, &backlogs, &snapshots, &muse).unwrap();
        assert_eq!(plan.dispatch[0].issue, issue);
    }

    #[test]
    fn an_unreadable_dispatch_state_holds_its_slot_and_its_issue() {
        // A drawer this binary cannot read (written by a newer one, or
        // corrupt) may be a live invocation: it is never skipped as free.
        let db = approved(&[REPO]);
        super::super::write_current(
            &db,
            &format!(
                "{}{}",
                dispatch_state::DISPATCH_STATE_KEY_PREFIX,
                IssueRef::new(REPO, 1).slug()
            ),
            r#"{"implementer":"some-future-harness"}"#,
        )
        .unwrap();
        let cfg = QueueConfig {
            concurrency_cap: 1,
            ..config()
        };
        let plan = plan_queue(
            &db,
            &[backlog(REPO, vec![ready(1), ready(2)])],
            &empty_registry(),
            &cfg,
        )
        .unwrap();
        assert_eq!(reason_for(&plan, 1), DeferReason::DispatchStateUnreadable);
        assert!(
            matches!(reason_for(&plan, 2), DeferReason::ConcurrencyCap { .. }),
            "{:?}",
            reason_for(&plan, 2)
        );
        assert!(plan.dispatch.is_empty());
        assert_eq!(plan.occupied_slots, 1);
    }
}
