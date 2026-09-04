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
use super::labels::{self, DispatchEligibility};
use super::registry::{Liveness, RegistrySnapshot};
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
    /// Compared against the issue's *cumulative* attempt count, matching
    /// [`super::run::RunConfig::attempt_cap`]. Checked here as well as in
    /// `run_issue` so an exhausted issue is reported as such by the queue
    /// rather than dispatched and immediately terminated.
    pub attempt_cap: u32,
    pub max_issues_per_repo: u32,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            concurrency_cap: DEFAULT_CONCURRENCY_CAP,
            daily_budget_usd: super::run::DEFAULT_DAILY_BUDGET_USD,
            max_budget_usd: super::run::DEFAULT_MAX_BUDGET_USD,
            attempt_cap: super::run::DEFAULT_ATTEMPT_CAP,
            max_issues_per_repo: DEFAULT_MAX_ISSUES_PER_REPO,
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
    /// as any other work.
    AlreadySucceeded,
    /// The per-issue attempt cap is spent.
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
}

impl QueuePlan {
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
    snapshot: &RegistrySnapshot,
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
    let in_flight: Vec<dispatch_state::DispatchState> =
        dispatch_state::all_dispatch_states(db, IN_FLIGHT_SCAN_LIMIT)?;

    // The same predicate `run_issue` uses, deliberately spelled the same way
    // rather than approximated. Note what this does *not* mirror: the
    // per-repo unpriced-dispatch ceiling rung 7 added. That one is conditional
    // on the repo having a wall-clock bound, and duplicating the condition
    // here to get it subtly wrong would be worse than letting `run_issue`
    // report it — which it does, terminally and without spending anything.
    let spent_today_usd = budget::get_daily_spend(db, &today_utc())?
        .map(|entry| entry.total_cost_usd)
        .unwrap_or(0.0);
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
            // to dispatch a succeeded issue again ("re-dispatch the IC to fix,
            // counting against the same per-issue attempt cap"), and the cap
            // check immediately below is what keeps that bounded rather than
            // endless — it is deliberately *not* skipped for a remediation.
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
            if cumulative_attempt_n >= config.attempt_cap {
                defer(DeferReason::AttemptCapReached {
                    cumulative_attempt_n,
                });
                continue;
            }

            if let Some(signature) = supervise::escalated_signature(db, &issue)? {
                defer(DeferReason::StrategyEscalated { signature });
                continue;
            }

            let state = in_flight.iter().find(|s| s.issue == issue);
            if let Some(state) = state {
                match snapshot.liveness(&state.ic_session_name) {
                    Liveness::Alive => {
                        defer(DeferReason::SessionLive {
                            session_name: state.ic_session_name.clone(),
                        });
                        continue;
                    }
                    Liveness::Unknown => {
                        defer(DeferReason::RegistryUnreadable {
                            detail: match snapshot {
                                RegistrySnapshot::Unavailable { reason } => reason.clone(),
                                // Unreachable: `Available` never answers
                                // `Unknown`. Spelled rather than
                                // `unreachable!` because a panic in the
                                // Lead's planning pass would take out every
                                // other repo's work too.
                                RegistrySnapshot::Available(_) => {
                                    "registry liveness was indeterminate".to_string()
                                }
                            },
                        });
                        continue;
                    }
                    Liveness::NotListed => {}
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
        let in_flight_after = in_flight.len() + selected.iter().filter(|s| !s.resuming).count();
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
        .count();

    Ok(QueuePlan {
        dispatch: selected,
        deferred,
        occupied_slots,
        concurrency_cap: config.concurrency_cap,
        spent_today_usd,
        daily_budget_usd: config.daily_budget_usd,
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
    use crate::autopilot::{lineage, DispatchState};

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
        dispatch_state::upsert_dispatch_state(
            db,
            &DispatchState {
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

    /// Arm a remediation on an issue whose success is recorded at `commit`.
    fn arm(db: &Database, issue: &IssueRef, commit: Option<&str>, cap: u32) {
        lineage::upsert_issue_status(
            db,
            &lineage::IssueStatus {
                issue: issue.clone(),
                cumulative_attempt_n: 1,
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: commit.map(str::to_string),
            },
        )
        .unwrap();
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
        // Five attempts spent since.
        put_status(&db, &issue, 5, true);

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
}
