//! Rung 10 — **the loop closes.**
//!
//! Rung 8's queue names ten reasons an issue is not dispatched. Nine of them
//! are transient or terminal-by-design: a budget rolls over, a cap frees, a
//! human re-labels. One is neither.
//! [`super::queue::DeferReason::AlreadySucceeded`] is reported on every tick,
//! forever, and until this module nothing anywhere acted on it.
//!
//! That is the open end of the spec's data flow. An IC goes green, pushes its
//! branch and opens a PR; `run_issue` records the success and clears the
//! dispatch-state drawer; and then the arc the spec draws — *"Lead dispatches
//! REVIEWER … PASS + low-risk + class matches → Lead merges … records
//! outcome, cleans worktree"* — simply stopped. Every piece of it was built:
//! rung 5 reviews and decides, rung 6 merges and labels. Nothing joined them,
//! so a green PR waited on a human to notice it and type two more commands.
//!
//! [`advance_pass`] is that join. For each issue whose lineage records a
//! success, it finds the open PR on the issue's branch, reviews it if no
//! review has read the PR's current head commit, applies rung 6's merge
//! authority, and — once the PR has landed — gives the worktree back.
//!
//! # What this module does not decide
//!
//! Deliberately almost everything. It runs no gate, judges no diff, and holds
//! no merge of its own: [`super::review::decide_merge`] is still the single
//! fail-closed answer to *"may this merge?"*, reached through
//! [`super::merge::execute_merge`] exactly as `autopilot merge` reaches it.
//! This module chooses only **which issue is at which step**, and it does so
//! from values it can point at — a label, a recorded verdict, two commit
//! SHAs compared for equality. There is no step in it where a language model
//! would be reading anything but its own guess, and so, like rungs 7 and 8,
//! it makes no model call at all.
//!
//! # Why it is not part of the Lead tick
//!
//! Rung 8 settled that *"a tick does not review or merge"*, because chaining
//! them would make the smallest unit of Lead activity the largest blast
//! radius. That holds, and nothing here changes `lead_tick`. `advance` is a
//! second, separately-authorized command that composes rungs 5 and 6 over the
//! issues rung 8's queue has finished with. An operator's cron runs the two
//! in order — `lead` starts work, `advance` finishes it — and each still
//! refuses on its own terms.
//!
//! # The two gates on the irreversible half
//!
//! Reviewing happens by default: it is already-authorized activity, bounded
//! by rung 5's own dollar and unpriced-call ceilings, and it is what makes
//! the merge decision available at all. **Merging does not.** Without
//! [`AdvanceConfig::merge`] the pass still runs [`super::merge::execute_merge`]
//! — every guard, every read — as a **rehearsal** (`dry_run`), so the
//! operator learns what would happen and GitHub is not written to. Merge is
//! the only irreversible action in the subsystem, and rung 9's precedent
//! applies: a bound that limits an already-authorized activity may default
//! on; a switch that decides whether a new kind of irreversible action
//! happens at all is an operator's explicit choice.
//!
//! # The other branch of the same fork ⟨rung 11⟩
//!
//! The reviewer's verdict is a fork, and rung 10 executed one side of it.
//! `PASS` merged; `NEEDS CHANGES` held the PR, commented, and moved the issue
//! to `agent:blocked` for a human. [`super::remediate`] executes the other
//! side, and this module is where it is decided, because this is where the
//! verdict is read.
//!
//! Under `--remediate`, a `needs_changes` verdict about the PR's **current
//! head** arms a re-dispatch instead of handing the PR over, and the merge is
//! then rehearsed rather than executed. That last part is the mechanism, not a
//! detail: rung 6's real hold sets `agent:blocked`, which strips `agent:ready`
//! and drops the issue out of the very listing [`advance_pass`] and the Lead
//! are built from — so a re-dispatch armed alongside it would never be
//! dispatched, and no question marker exists for `blocked::poll_answer` to
//! recover it by.
//!
//! The hand-off to a human still happens; it happens *later*, when the
//! automation has actually given up. `remediate::arm_remediation` checks the
//! attempt cap first and answers `CapReached`, which is not `in_force`, so an
//! exhausted issue falls through to the real merge and rung 6 comments and
//! labels exactly as it always did. That is the spec's *"on exhaustion the PR
//! stays open for a human — never merged with an unresolved finding"*, with
//! both halves reachable.
//!
//! # No eleventh drawer kind
//!
//! Every action *this* module takes is already recorded by the rung that owns
//! it: rung 5 writes a review record, rung 6 writes a merge record, and both
//! dedupe their own repeats. A record of *"I orchestrated"* would answer no
//! question the two of them cannot already answer, and rung 9's rule for the
//! tenth kind was that a kind earns its place by making a specific question
//! answerable later. This one does not.
//!
//! ⟨rung 11⟩ [`super::remediate::RemediationRecord`] *is* an eleventh kind,
//! and it earns its place by that same rule: it is not a record of an action
//! taken, it is the state two other modules read to decide whether to act at
//! all. Nothing else in the subsystem can answer *"which findings is this
//! dispatch supposed to be fixing?"*.
//!
//! # Ordering, which is this ladder's recurring bug class
//!
//! Within one issue: **look up the PR → review → merge → clean up.** The
//! cleanup is last and every part of it is idempotent, because by then the
//! merge has happened and cannot be undone — a cleanup failure is reported,
//! never propagated, exactly as rung 6 treats a label write that fails after
//! a merge lands.
//!
//! The worktree is removed **after the PR lands, never when the IC goes
//! green**, and the two facts are the same fact: the worktree is the
//! reviewer's input. Removing it at success would delete the checkout the
//! next step reads, and a reviewer pointed at a checkout that does not
//! contain the branch does not fail — it writes a confident review of
//! something else.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::gh::{self, GhRunner, MergeStrategy, PrLookup};
use super::labels::{self, AgentLabel, DispatchEligibility};
use super::lineage::{self, AttemptOutcome};
use super::merge::{self, serialize_issue, MergeExecution, MergeRequest};
use super::remediate::{self, ArmOutcome, ArmRequest};
use super::review::{self, PrReview, ReviewRequest, ReviewRunner, ReviewVerdict};
use super::worktree::{self, WorktreeRemoval};
use super::{dispatch_state, gate_config, queue, validate_repo, IssueRef};

/// How many issues one pass will carry a step forward.
///
/// A bound on a pass, not a persisted counter: it exists so a cron entry that
/// fires every few minutes cannot start twenty reviews at once on the morning
/// after a productive night. Rung 5's ceilings still bound the *day*; this
/// bounds the burst.
pub const DEFAULT_MAX_ADVANCES_PER_PASS: usize = 3;

/// Policy for one advance pass.
#[derive(Debug, Clone)]
pub struct AdvanceConfig {
    /// The repos to look at, and where each is checked out.
    pub targets: Vec<super::lead::RepoTarget>,
    /// How many `agent:ready` issues to list per repo.
    pub max_issues_per_repo: u32,
    /// See [`DEFAULT_MAX_ADVANCES_PER_PASS`].
    pub max_advances_per_pass: usize,
    /// Whether merges are executed. Off means every merge is rehearsed.
    pub merge: bool,
    /// Whether a `needs_changes` verdict arms a re-dispatch (rung 11).
    ///
    /// Off by default, and off is exactly rung 10's behaviour: the PR is held,
    /// commented and flipped to `agent:blocked` for a human. On, the same
    /// verdict instead re-opens the issue for the IC that wrote it, and the
    /// merge is rehearsed rather than executed so the issue keeps the
    /// `agent:ready` label the Lead dispatches from.
    ///
    /// A switch rather than a default because turning it on changes what an
    /// existing `advance --merge` cron does: it re-opens work a human may
    /// believe is finished. A new optional feature must not alter the
    /// behaviour of a configuration that predates it (rung 9's lesson 43),
    /// and rungs 9 and 10 set the precedent with `--advisor` and `--merge`.
    /// The bounds on how much a remediation may spend are separate and
    /// always on.
    ///
    /// It is deliberately *not* justified as a guard against a human pushing
    /// to the branch at the same time: the spec records the operating
    /// assumption that this does not happen, and a flag would be a poor guard
    /// against it in any case.
    pub remediate: bool,
    /// The per-issue attempt cap remediations are dispatched under.
    ///
    /// Must be the value the Lead's [`super::run::RunConfig`] uses. A
    /// remediation armed against one cap and dispatched under another either
    /// never fires or never stops, and this module is the half that decides
    /// when the automation gives up and tells a human.
    pub attempt_cap: u32,
    pub strategy: MergeStrategy,
    pub delete_branch: bool,
    /// Change nothing anywhere: no merge, no comment, no label, no cleanup.
    /// Implies a rehearsed merge regardless of [`AdvanceConfig::merge`].
    pub dry_run: bool,
    pub daily_budget_usd: f64,
    pub max_unpriced_reviews_per_day: u32,
    /// Where [`super::worktree::worktree_path`] resolves issue checkouts.
    pub worktree_root: PathBuf,
}

impl AdvanceConfig {
    /// Reject a configuration whose bounds cannot bind.
    ///
    /// The NaN check is rung 5's, spelled the same way rather than
    /// approximated: a NaN ceiling makes every `spent >= budget` comparison
    /// false, so the bound does not fail closed — it silently disappears.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if !self.daily_budget_usd.is_finite() || self.daily_budget_usd <= 0.0 {
            return Err(MemoryError::Config(
                "daily_budget_usd must be a finite, positive number".into(),
            ));
        }
        if self.max_advances_per_pass == 0 {
            return Err(MemoryError::Config(
                "max_advances_per_pass must be at least 1".into(),
            ));
        }
        // Checked whether or not `remediate` is set. A cap of zero is a
        // nonsense configuration however it is read, and validating it only
        // under the new flag would let the same value pass one command line
        // and fail the next.
        if self.attempt_cap == 0 {
            return Err(MemoryError::Config("attempt_cap must be at least 1".into()));
        }
        for target in &self.targets {
            validate_repo(&target.repo)?;
        }
        Ok(())
    }
}

/// Why an issue in a backlog was not carried forward.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum SkipReason {
    /// No approved gate config, so there are no gate commands to build a
    /// review prompt from. Named here so the operator reads "onboard this
    /// repo" rather than a per-issue error.
    RepoNotApproved,
    /// Rung 6's label eligibility said no. An `agent:exhausted` or
    /// `agent:blocked` issue is one a human has taken back.
    NotEligible { eligibility: DispatchEligibility },
    /// Lineage records no success yet, so there is nothing to advance. This
    /// is the ordinary case for most of a backlog, and it is the exact
    /// complement of the queue's `AlreadySucceeded`.
    NoSuccessYet,
    /// The backlog names a repo no `--repo` target gives a checkout for, so
    /// there is nowhere to read a worktree from. Distinct from
    /// [`SkipReason::RepoNotApproved`], which would tell an operator to
    /// onboard a repo that is already onboarded.
    NoCheckout,
    /// The pass's own burst limit was reached before this issue's turn.
    PassLimitReached { limit: usize },
}

/// An issue that could not be carried forward, and why.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Skipped {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub reason: SkipReason,
}

/// A succeeded issue that cannot move, and what is in the way.
///
/// Every variant is a fact about the world rather than a failure of this
/// module, and every one is reported on each pass. That repetition is
/// deliberate: the fix for each is a human action, and an operator who stops
/// being told stops acting.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "stall", rename_all = "snake_case")]
pub enum Stall {
    /// No open PR on the issue's branch. The IC recorded a success and never
    /// pushed one, a human closed it, or it has already been merged and the
    /// issue left open.
    NoOpenPr { branch: String },
    /// More than one open PR shares the branch. GitHub permits it, and the
    /// candidates can target different bases, so picking one would mean
    /// merging a PR nobody named. See [`super::gh::PrLookup::Ambiguous`].
    AmbiguousPr { numbers: Vec<u64> },
    /// A review is needed and the issue's worktree is gone.
    ///
    /// **Fails closed rather than falling back to the main checkout.** The
    /// reviewer reads a diff from the checkout it is pointed at; one that
    /// does not contain the branch yields a confident review of the wrong
    /// thing, and that review is what authorizes a merge. Paying for it
    /// would be worse than paying for nothing.
    WorktreeMissing { path: String },
}

/// The step an issue is at.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum AdvanceStep {
    /// The PR's current head has never been reviewed.
    Review {
        pr_number: u64,
        head_sha: String,
        base_branch: String,
        gate_green: bool,
    },
    /// A review has read this exact head commit; rung 6 decides from here.
    Merge {
        pr_number: u64,
        head_sha: String,
        gate_green: bool,
    },
    /// Something a human has to resolve.
    Stalled(Stall),
}

/// What one issue's pass did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Advanced {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    /// The class the review and the merge decision ran under. See
    /// [`dispatch_class`].
    pub dispatch_class: String,
    pub step: AdvanceStep,
    pub review: Option<PrReview>,
    /// What rung 11 did about a `needs_changes` verdict. `None` when the
    /// verdict was not `needs_changes`, or when `--remediate` is off.
    pub remediation: Option<ArmOutcome>,
    pub merge: Option<MergeExecution>,
    pub cleanup: Option<Cleanup>,
}

/// Terminal bookkeeping for an issue whose PR has landed.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cleanup {
    /// Whether a dispatch-state drawer was still present and was removed.
    ///
    /// Normally false: `run_issue` clears it when the IC goes green. It is
    /// attempted anyway because the paths that keep a drawer — a run paused
    /// on the daily budget, an infrastructure failure, an escalation — can
    /// all be followed by a later green run, and a drawer left behind holds a
    /// concurrency slot against work that has landed.
    pub dispatch_state_cleared: bool,
    pub worktree: WorktreeRemoval,
    /// A cleanup failure, reported rather than propagated: the merge has
    /// already happened, and losing the report of an irreversible action
    /// because the tidying failed is rung 6's mistake, not one to repeat.
    pub error: Option<String>,
}

/// Something that went wrong on one issue, which the pass survived.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdvanceProblem {
    pub what: String,
    pub detail: String,
}

/// One advance pass.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AdvanceReport {
    pub dry_run: bool,
    /// Whether merges were executed or rehearsed.
    pub merge_enabled: bool,
    /// Whether a `needs_changes` verdict re-opened the issue (rung 11) or was
    /// held for a human (rung 10's behaviour).
    pub remediate_enabled: bool,
    pub advanced: Vec<Advanced>,
    pub skipped: Vec<Skipped>,
    /// Per-issue and per-repo failures. A pass that reports problems still
    /// advanced everything it could: one repo's `gh` outage must not strand
    /// another repo's merged PR.
    pub problems: Vec<AdvanceProblem>,
}

/// An issue whose lineage records a success and whose labels still opt in.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub issue: IssueRef,
    /// Where the repo is checked out.
    pub repo_path: PathBuf,
    /// The `risk:*` label's value, by [`super::queue::risk_label`]'s rules.
    pub risk_label: Option<String>,
    /// The commit the gate was green at.
    pub green_commit_sha: Option<String>,
}

/// The dispatch class an advance runs under.
///
/// **The `risk:*` label, or `unclassified`.** Rung 8's `class_for`, with no
/// new resolution rule and — unlike rung 9's `resolve_class` — no advisor
/// call.
///
/// Two reasons not to ask a model here. The cheap one is lesson 34: by this
/// point the work is done and reviewed, and a class is either stated by a
/// label or absent. The load-bearing one is that this value is half of
/// `decide_merge`'s `ClassMismatch` test, so it is precisely the input that
/// decides whether a PR may merge **without a human**. Requiring a human to
/// have written `risk:documentation` before that can happen is not a gap in
/// the automation; it is the authorization.
///
/// The dispatch-time class recorded in the dispatch-state drawer is
/// deliberately not consulted: `run_issue` clears that drawer when the IC
/// goes green, so reading it would make the class depend on whether an
/// unrelated cleanup had run.
pub fn dispatch_class(risk_label: Option<&str>) -> String {
    super::lead::class_for(risk_label, super::lead::UNCLASSIFIED)
}

/// Choose the issues one pass will look at.
///
/// Pure over the database and `backlogs` — no network, no writes — for the
/// reason [`super::queue::plan_queue`] is: the choosing is then testable
/// exhaustively without a GitHub call.
///
/// Rung 7's escalation flag is deliberately **not** consulted. An escalation
/// says an approach is not converging; a recorded success settles that
/// question, and holding a green PR behind a stale flag would strand landed
/// work behind a warning about work that is finished.
pub fn plan_advance(
    db: &Database,
    backlogs: &[queue::RepoBacklog],
    config: &AdvanceConfig,
) -> Result<(Vec<Candidate>, Vec<Skipped>), MemoryError> {
    config.validate()?;
    let mut candidates = Vec::new();
    let mut skipped = Vec::new();

    for backlog in backlogs {
        validate_repo(&backlog.repo)?;
        let repo_approved = gate_config::is_gate_config_approved(db, &backlog.repo)?;
        let repo_path = config
            .targets
            .iter()
            .find(|t| t.repo == backlog.repo)
            .map(|t| t.path.clone());

        for listing in &backlog.issues {
            let issue = IssueRef::new(backlog.repo.clone(), listing.number);
            if !repo_approved {
                skipped.push(Skipped {
                    issue,
                    reason: SkipReason::RepoNotApproved,
                });
                continue;
            }
            let eligibility = labels::eligibility(&listing.labels);
            if !eligibility.is_eligible() {
                skipped.push(Skipped {
                    issue,
                    reason: SkipReason::NotEligible { eligibility },
                });
                continue;
            }
            let status = lineage::get_issue_status(db, &issue)?;
            let Some(status) = status.filter(|s| s.best_verdict == Some(AttemptOutcome::Success))
            else {
                skipped.push(Skipped {
                    issue,
                    reason: SkipReason::NoSuccessYet,
                });
                continue;
            };
            // A backlog for a repo with no target is unreachable through
            // `advance_pass` (the backlogs are built from the targets), but
            // the function is public and pure, so it must not silently
            // invent a path.
            let Some(repo_path) = repo_path.clone() else {
                skipped.push(Skipped {
                    issue,
                    reason: SkipReason::NoCheckout,
                });
                continue;
            };
            candidates.push(Candidate {
                issue,
                repo_path,
                risk_label: queue::risk_label(&listing.labels),
                green_commit_sha: status.best_commit_sha,
            });
        }
    }

    // Oldest issue first, so a pass truncated by the burst limit works
    // through the backlog in a stable order instead of re-picking whichever
    // repo GitHub listed first. The key is immutable, rung 8's rule.
    candidates.sort_by(|a, b| {
        a.issue
            .repo
            .cmp(&b.issue.repo)
            .then(a.issue.number.cmp(&b.issue.number))
    });
    Ok((candidates, skipped))
}

/// Which step `candidate` is at, given the PR lookup's answer.
///
/// Pure over the database and `lookup`.
///
/// The review trigger is **"no review has read this head commit"**, not "no
/// review exists". It is the same equality rung 6 enforces before merging —
/// a review authorizes the commit it read and no other — so an IC that
/// pushed a fix is re-reviewed automatically, and a PR whose head has not
/// moved is never re-reviewed and never re-billed.
pub fn next_step(
    db: &Database,
    candidate: &Candidate,
    lookup: &PrLookup,
    worktree_root: &Path,
) -> Result<AdvanceStep, MemoryError> {
    let pr = match lookup {
        PrLookup::None => {
            return Ok(AdvanceStep::Stalled(Stall::NoOpenPr {
                branch: worktree::branch_name(&candidate.issue),
            }))
        }
        PrLookup::Ambiguous { numbers } => {
            return Ok(AdvanceStep::Stalled(Stall::AmbiguousPr {
                numbers: numbers.clone(),
            }))
        }
        PrLookup::Found(pr) => pr,
    };

    // The gate was green at *some* commit. Asserting it is green at the PR's
    // head is only true when they are the same commit — a branch with
    // commits pushed after the green run has unverified code at its head, and
    // `decide_merge` requires green for the auto-merge it authorizes.
    //
    // An absent green SHA answers false, not true: unknown fails closed.
    //
    // Compared case-insensitively, the way `merge::evaluate` compares the
    // same two values. A differently-cased spelling of one commit is one
    // commit, and treating it as two would answer "not green" about a green
    // gate — silently, and while looking healthy.
    let gate_green = candidate
        .green_commit_sha
        .as_deref()
        .is_some_and(|sha| !sha.is_empty() && sha.eq_ignore_ascii_case(&pr.head_sha));

    // The base is compared as well as the head, for the reason rung 6
    // compares both before merging: a PR retargeted after its review was
    // reviewed against a diff that no longer exists. Head-only, a retargeted
    // PR reports as already reviewed forever — its head never moves, so no
    // pass ever re-reviews it — and `decide_merge` holds it at
    // `BaseBranchMismatch` with no way out. `None` is a review that predates
    // the field: unknown, so the comparison is skipped rather than read as a
    // mismatch, exactly as `merge::evaluate` skips it.
    let reviewed_this_head = review::reviews_for_issue(db, &candidate.issue)?
        .iter()
        .any(|r| {
            r.pr_number == pr.number
                && r.head_sha
                    .as_deref()
                    .is_some_and(|sha| sha.eq_ignore_ascii_case(&pr.head_sha))
                && {
                    let reviewed_base = r.base_branch.as_deref().unwrap_or_default();
                    reviewed_base.is_empty() || reviewed_base == pr.base_branch
                }
        });

    if reviewed_this_head {
        return Ok(AdvanceStep::Merge {
            pr_number: pr.number,
            head_sha: pr.head_sha.clone(),
            gate_green,
        });
    }

    let path = worktree::worktree_path(worktree_root, &candidate.issue);
    if !path.exists() {
        return Ok(AdvanceStep::Stalled(Stall::WorktreeMissing {
            path: path.to_string_lossy().to_string(),
        }));
    }

    Ok(AdvanceStep::Review {
        pr_number: pr.number,
        head_sha: pr.head_sha.clone(),
        base_branch: pr.base_branch.clone(),
        gate_green,
    })
}

/// Run one advance pass across every configured repo.
///
/// Per-issue failures are collected into [`AdvanceReport::problems`] and the
/// pass continues. A `gh` outage on one repo, or one issue whose review will
/// not launch, must not strand another issue's merged PR — the whole point of
/// a pass that runs unattended is that it makes whatever progress it can.
pub fn advance_pass(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    reviewer: &mut dyn ReviewRunner,
    config: &AdvanceConfig,
) -> Result<AdvanceReport, MemoryError> {
    config.validate()?;
    let mut problems = Vec::new();

    let backlogs = fetch_backlogs(gh_runner, config, &mut problems);
    let (candidates, mut skipped) = plan_advance(db, &backlogs, config)?;

    let mut advanced = Vec::new();
    // Counted separately from `advanced.len()`, because a stall is not a step
    // taken. A stall is a fact about the world that only a human can change —
    // an issue left `agent:ready` after its PR was merged and closed, two open
    // PRs on one branch — so it is reported again on every pass, forever.
    // Charging those repeats against the burst limit let three of them fill it
    // permanently, and every other green PR behind them was then never
    // reviewed, never merged, and never mentioned as anything but "deferred".
    let mut carried = 0usize;
    for candidate in candidates {
        if carried >= config.max_advances_per_pass {
            skipped.push(Skipped {
                issue: candidate.issue.clone(),
                reason: SkipReason::PassLimitReached {
                    limit: config.max_advances_per_pass,
                },
            });
            continue;
        }
        match advance_issue(db, gh_runner, reviewer, &candidate, config) {
            Ok(step) => {
                if took_a_step(&step) {
                    carried += 1;
                }
                advanced.push(step);
            }
            Err(e) => problems.push(AdvanceProblem {
                what: format!("advance {}", candidate.issue.canonical()),
                detail: e.to_string(),
            }),
        }
    }

    Ok(AdvanceReport {
        dry_run: config.dry_run,
        merge_enabled: config.merge && !config.dry_run,
        remediate_enabled: config.remediate && !config.dry_run,
        advanced,
        skipped,
        problems,
    })
}

/// Whether this pass actually carried the issue forward, as against
/// re-reporting a condition it could not act on.
///
/// Only what the burst limit is spent on — rung 10's lesson 52, *a burst limit
/// must be spent by work, not by reports*, which cost that rung a MEDIUM
/// finding when stalls were charged against it.
///
/// A stall is the permanent form. ⟨rung 11⟩ An **already-armed remediation is
/// the transient form**, and it needs the same treatment: its head has not
/// moved, so no review was bought, and the merge was rehearsed — the pass did
/// nothing to it but say so. An IC can take several dispatches to push a fix,
/// and with the default limit of three, three remediating issues would fill
/// every pass for that whole window and starve every green PR behind them of
/// the review and merge they were ready for.
///
/// A *newly* armed remediation is not free and is charged: reaching it meant
/// either paying for the review that produced the verdict, or reading a stored
/// one and writing the record that re-opens the issue.
fn took_a_step(advanced: &Advanced) -> bool {
    if matches!(advanced.step, AdvanceStep::Stalled(_)) {
        return false;
    }
    !(advanced.review.is_none()
        && matches!(advanced.remediation, Some(ArmOutcome::AlreadyArmed { .. })))
}

/// List each target repo's `agent:ready` issues.
///
/// An unreadable repo is a reported problem, never an empty backlog: rung 7's
/// lesson 21 at repo granularity. An empty listing here would claim a repo
/// has no landed work, and the pass would act on that claim by cleaning up
/// nothing and telling nobody.
fn fetch_backlogs(
    gh_runner: &mut dyn GhRunner,
    config: &AdvanceConfig,
    problems: &mut Vec<AdvanceProblem>,
) -> Vec<queue::RepoBacklog> {
    let mut backlogs = Vec::new();
    for target in &config.targets {
        match gh::list_labeled_issues(
            gh_runner,
            &target.repo,
            AgentLabel::Ready.as_str(),
            config.max_issues_per_repo,
        ) {
            Ok(issues) => backlogs.push(queue::RepoBacklog {
                repo: target.repo.clone(),
                issues,
            }),
            Err(e) => problems.push(AdvanceProblem {
                what: format!("list {} ready issues", target.repo),
                detail: e.to_string(),
            }),
        }
    }
    backlogs
}

/// Carry one issue forward by exactly one step.
///
/// Ordering: look up the PR, review, merge, clean up. Each stage's output is
/// the next one's input, and the irreversible one is third of four so the
/// bookkeeping that follows it can fail without erasing it.
fn advance_issue(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    reviewer: &mut dyn ReviewRunner,
    candidate: &Candidate,
    config: &AdvanceConfig,
) -> Result<Advanced, MemoryError> {
    let branch = worktree::branch_name(&candidate.issue);
    let lookup = gh::open_pr_for_branch(gh_runner, &candidate.issue.repo, &branch)?;
    let step = next_step(db, candidate, &lookup, &config.worktree_root)?;
    let class = dispatch_class(candidate.risk_label.as_deref());

    let (pr_number, head_sha, gate_green) = match &step {
        AdvanceStep::Stalled(_) => {
            return Ok(Advanced {
                issue: candidate.issue.clone(),
                dispatch_class: class,
                step,
                review: None,
                remediation: None,
                merge: None,
                cleanup: None,
            })
        }
        AdvanceStep::Review {
            pr_number,
            head_sha,
            gate_green,
            ..
        }
        | AdvanceStep::Merge {
            pr_number,
            head_sha,
            gate_green,
        } => (*pr_number, head_sha.clone(), *gate_green),
    };

    let mut review = None;
    if let AdvanceStep::Review { base_branch, .. } = &step {
        if config.dry_run {
            // A rehearsal does not spend money. Reviewing is the one paid
            // step here, and `--dry-run` means "change nothing" — a ledger
            // entry and a review drawer are changes.
            return Ok(Advanced {
                issue: candidate.issue.clone(),
                dispatch_class: class,
                step,
                review: None,
                remediation: None,
                merge: None,
                cleanup: None,
            });
        }
        let gate_commands = super::run::approved_gate_commands(db, &candidate.issue.repo)?;
        let repo_dir = worktree::worktree_path(&config.worktree_root, &candidate.issue);
        let mut done = review::review_pr(
            db,
            reviewer,
            &ReviewRequest {
                issue: &candidate.issue,
                pr_number,
                base_branch,
                head_branch: &branch,
                // Pinned to the SHA the listing reported rather than
                // re-resolved from the checkout. The checkout can be behind
                // the remote, and a review recorded against a stale SHA is
                // one rung 6 will refuse to merge — silently, and while
                // looking healthy.
                head_sha: Some(head_sha.clone()),
                dispatch_class: &class,
                gate_commands: &gate_commands,
                gate_green,
                repo_dir: &repo_dir,
                daily_budget_usd: config.daily_budget_usd,
                max_unpriced_reviews_per_day: config.max_unpriced_reviews_per_day,
            },
        )?;
        // Scrubbed on the way out for the reason `autopilot review` scrubs
        // it: a review reason quotes the diff and can carry whatever the diff
        // carried, and this string reaches stdout and any log the caller
        // keeps. The drawer's own redaction flag still records that a
        // redaction happened on the storage path.
        if let Some(reason) = done.outcome.reason.take() {
            done.outcome.reason =
                Some(super::scrub::scrub_and_bound(&reason, lineage::MAX_LINEAGE_FIELD_CHARS).text);
        }
        review = Some(done);
    }

    // A review the day's ceilings *refused* is not a verdict, and the merge
    // must not be reached over it. `review_pr` records nothing when it
    // refuses, so `execute_merge` would find no review at this head, hold
    // `NotReviewed`, and — under `--merge` — comment and move the issue to
    // `agent:blocked`. That label strips `agent:ready`, so the issue leaves
    // this pass's listing, and a merge hold carries no question marker, so
    // `blocked::poll_answer` never resumes it either: a budget that rolled
    // over would need a human to unpick every green issue by hand. The
    // refusal's own contract is "retry when the day rolls over", so the issue
    // is left exactly where it was.
    if review.as_ref().is_some_and(|r| r.refusal.is_some()) {
        return Ok(Advanced {
            issue: candidate.issue.clone(),
            dispatch_class: class,
            step,
            review,
            remediation: None,
            merge: None,
            cleanup: None,
        });
    }

    // ── Rung 11: the red path ────────────────────────────────────────────
    //
    // Read back from the *stored* review rather than from the `PrReview` in
    // hand, for the reason rung 6 re-derives its merge decision instead of
    // replaying a stored one: the stored review is what `execute_merge` is
    // about to read, and two sources of truth for "what did the reviewer
    // say?" is how they drift apart. It is also the only source available on
    // the `Merge` step, where this pass reviewed nothing at all.
    //
    // The head-SHA comparison is the same one rung 6 makes before merging and
    // rung 10 makes before re-reviewing: a verdict authorizes — or objects to
    // — the commit it read, and a `needs_changes` about some earlier commit is
    // not a reason to re-open the one in front of us.
    let mut remediation = None;
    if config.remediate && !config.dry_run {
        let objection = merge::latest_review_for_pr(db, &candidate.issue, pr_number)?.filter(|r| {
            r.verdict == Some(ReviewVerdict::NeedsChanges)
                && r.head_sha
                    .as_deref()
                    .is_some_and(|sha| sha.eq_ignore_ascii_case(&head_sha))
        });
        if let Some(objection) = objection {
            remediation = Some(remediate::arm_remediation(
                db,
                &ArmRequest {
                    issue: &candidate.issue,
                    pr_number,
                    head_sha: &head_sha,
                    findings: objection.reason.as_deref(),
                    attempt_cap: config.attempt_cap,
                },
            )?);
        }
    }

    // Rehearsed unless merging was asked for. Every guard and every read
    // still runs, so the operator learns what would happen; nothing is
    // written to GitHub.
    //
    // A remediation in force forces the rehearsal too, and that is the whole
    // mechanism rather than a nicety. Under `--merge`, rung 6's `NeedsChanges`
    // hold comments on the issue and sets `agent:blocked` — which strips
    // `agent:ready`, drops the issue out of the Lead's listing, and carries no
    // question marker for `blocked::poll_answer` to resume it by. The
    // re-dispatch this module just armed would then never be dispatched, and
    // only a human re-labelling could recover it. Holding for a human is the
    // right answer only once the automation has given up, which is exactly
    // what `ArmOutcome::CapReached` reports — and it is not `in_force`, so it
    // falls through to the real merge and the human is told.
    //
    // Confirmed against [`remediate::active_remediation`] rather than taken
    // from `ArmOutcome` alone, because the two answer different questions and
    // can disagree. `ArmOutcome::AlreadyArmed` says only *"a record exists for
    // this (pr, commit)"*; `active_remediation` is the definition the Lead's
    // `plan_queue` and `run_issue` actually dispatch from, and it also asks
    // whether a newer success has superseded the record. An IC that commits
    // locally and fails to push moves `best_commit_sha` past `armed_at_commit`
    // while the PR head stays exactly where the reviewer left it: the Lead
    // then defers `AlreadySucceeded` and never re-dispatches, while this
    // module would answer `AlreadyArmed` and rehearse the merge on every pass
    // for ever — an issue neither merged, nor re-opened, nor handed to a
    // human, and `took_a_step` charges nothing for saying so. Reading the same
    // function the Lead reads keeps the rehearsal tied to a re-dispatch that
    // will actually happen, and lets the exhausted case reach rung 6's real
    // hold.
    let remediation_in_force = remediation.as_ref().is_some_and(ArmOutcome::in_force)
        && remediate::active_remediation(db, &candidate.issue)?.is_some();
    let merge_dry_run = config.dry_run || !config.merge || remediation_in_force;
    let execution = merge::execute_merge(
        db,
        gh_runner,
        &MergeRequest {
            issue: &candidate.issue,
            pr_number,
            gate_green,
            strategy: config.strategy,
            delete_branch: config.delete_branch,
            dry_run: merge_dry_run,
        },
    )?;

    // Only for a PR that has actually landed, and never during a rehearsal.
    let cleanup = if execution.outcome.landed() && !merge_dry_run {
        Some(clean_up(db, candidate, config))
    } else {
        None
    };

    Ok(Advanced {
        issue: candidate.issue.clone(),
        dispatch_class: class,
        step,
        review,
        remediation,
        merge: Some(execution),
        cleanup,
    })
}

/// Give back what a landed issue was holding.
///
/// Never returns `Err`. The merge is done by the time this runs, and a
/// cleanup that fails must not take the record of an irreversible action with
/// it — rung 6's rule for a label write that fails after a merge, applied to
/// the two things that outlive one.
///
/// Both halves are idempotent, because a pass can reach this twice: a merge
/// whose label write failed leaves `agent:ready` in place, so the next pass
/// lists the issue again, reads the PR as `AlreadyMerged`, and cleans up a
/// second time.
///
/// **It is not, however, a retry loop.** A merge whose label write *succeeded*
/// clears every `agent:*` label, so the issue drops out of the `agent:ready`
/// listing this pass is built from and nothing brings it back. Whatever this
/// call leaves behind — a refused dirty worktree, a drawer a storage error
/// kept — is left behind for good, which is why the error is reported on
/// stdout rather than only stored.
fn clean_up(db: &Database, candidate: &Candidate, config: &AdvanceConfig) -> Cleanup {
    let mut error = None;

    let dispatch_state_cleared = match dispatch_state::clear_dispatch_state(db, &candidate.issue) {
        Ok(cleared) => cleared,
        Err(e) => {
            error = Some(format!("could not clear the dispatch state: {e}"));
            false
        }
    };

    let worktree = match worktree::remove_worktree(
        &candidate.repo_path,
        &config.worktree_root,
        &candidate.issue,
    ) {
        Ok(removal) => removal,
        Err(e) => {
            let detail = format!("could not remove the worktree: {e}");
            error = Some(match error {
                Some(first) => format!("{first}; {detail}"),
                None => detail,
            });
            WorktreeRemoval::Absent
        }
    };

    Cleanup {
        dispatch_state_cleared,
        worktree,
        error,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::GhOutput;
    use crate::autopilot::lineage::IssueStatus;
    use crate::autopilot::merge::MergeOutcome;
    use crate::autopilot::review::{ReviewOutcome, ReviewVerdict, RiskClass};
    use crate::autopilot::{gate_config, DispatchState};

    const REPO: &str = "ironrace/ironmem";
    const GREEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const MOVED: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn issue() -> IssueRef {
        IssueRef::new(REPO, 283)
    }

    fn approved_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        gate_config::propose_gate_config(
            &db,
            REPO,
            vec!["cargo test --workspace".to_string()],
            Vec::new(),
        )
        .unwrap();
        gate_config::approve_gate_config(&db, REPO).unwrap();
        db
    }

    /// Record the lineage success that makes an issue a candidate.
    fn record_success(db: &Database, issue: &IssueRef, commit_sha: Option<&str>) {
        lineage::upsert_issue_status(
            db,
            &IssueStatus {
                issue: issue.clone(),
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: commit_sha.map(str::to_string),
                cumulative_attempt_n: 1,
            },
        )
        .unwrap();
    }

    /// Record a passing review of `pr_number` at `head_sha`, which is what
    /// makes an issue's next step the merge rather than another review.
    fn reviewed_at(db: &Database, issue: &IssueRef, pr_number: u64, head_sha: &str) {
        review::record_review(
            db,
            &review::ReviewRecord {
                issue: issue.clone(),
                pr_number,
                dispatch_class: "documentation".into(),
                head_sha: Some(head_sha.into()),
                base_branch: Some("main".into()),
                outcome: ReviewOutcome {
                    verdict: Some(ReviewVerdict::Pass),
                    risk_class: Some(RiskClass::Documentation),
                    reason: None,
                    total_cost_usd: None,
                    token_usage: None,
                    process_success: true,
                },
                decision: review::MergeDecision::EligibleForMerge {
                    class: RiskClass::Documentation,
                },
            },
        )
        .unwrap();
    }

    /// A recorded `needs_changes` review of `pr_number` at `head_sha`.
    fn reviewed_at_needing_changes(
        db: &Database,
        issue: &IssueRef,
        pr_number: u64,
        head_sha: &str,
    ) {
        review::record_review(
            db,
            &review::ReviewRecord {
                issue: issue.clone(),
                pr_number,
                dispatch_class: "documentation".into(),
                head_sha: Some(head_sha.into()),
                base_branch: Some("main".into()),
                outcome: ReviewOutcome {
                    verdict: Some(ReviewVerdict::NeedsChanges),
                    risk_class: Some(RiskClass::Documentation),
                    reason: Some("the retry loop is unbounded".to_string()),
                    total_cost_usd: None,
                    token_usage: None,
                    process_success: true,
                },
                decision: review::MergeDecision::HoldForHuman(
                    crate::autopilot::review::HoldReason::NeedsChanges,
                ),
            },
        )
        .unwrap();
    }

    fn listing(number: u64, labels: &[&str]) -> gh::IssueListing {
        serde_json::from_str(&format!(
            r#"{{"number":{number},"title":"t","body":"b","labels":[{}],"updatedAt":"2026-09-03T00:00:00Z"}}"#,
            labels
                .iter()
                .map(|l| format!(r#"{{"name":"{l}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        ))
        .unwrap()
    }

    fn backlog(issues: Vec<gh::IssueListing>) -> Vec<queue::RepoBacklog> {
        vec![queue::RepoBacklog {
            repo: REPO.to_string(),
            issues,
        }]
    }

    fn config(worktree_root: &Path, repo_path: &Path) -> AdvanceConfig {
        AdvanceConfig {
            targets: vec![super::super::lead::RepoTarget {
                repo: REPO.to_string(),
                path: repo_path.to_path_buf(),
                base: "HEAD".to_string(),
            }],
            max_issues_per_repo: 50,
            max_advances_per_pass: DEFAULT_MAX_ADVANCES_PER_PASS,
            merge: false,
            remediate: false,
            attempt_cap: crate::autopilot::run::DEFAULT_ATTEMPT_CAP,
            strategy: MergeStrategy::Squash,
            delete_branch: false,
            dry_run: false,
            daily_budget_usd: 25.0,
            max_unpriced_reviews_per_day: 20,
            worktree_root: worktree_root.to_path_buf(),
        }
    }

    fn found(number: u64, head_sha: &str) -> PrLookup {
        PrLookup::Found(gh::PrForBranch {
            number,
            head_branch: worktree::branch_name(&issue()),
            head_sha: head_sha.to_string(),
            base_branch: "main".to_string(),
            is_draft: false,
            url: String::new(),
        })
    }

    /// A reviewer that records its calls and returns a canned verdict.
    struct StubReviewer {
        calls: usize,
        outcome: ReviewOutcome,
    }

    impl StubReviewer {
        fn passing() -> Self {
            Self {
                calls: 0,
                outcome: ReviewOutcome {
                    verdict: Some(ReviewVerdict::Pass),
                    risk_class: Some(RiskClass::Documentation),
                    reason: Some("looks right".to_string()),
                    total_cost_usd: None,
                    token_usage: None,
                    process_success: true,
                },
            }
        }
    }

    impl ReviewRunner for StubReviewer {
        fn review(
            &mut self,
            _repo_dir: &Path,
            _prompt: &str,
        ) -> Result<ReviewOutcome, MemoryError> {
            self.calls += 1;
            Ok(self.outcome.clone())
        }
    }

    /// A reviewer that must never be called.
    struct ForbiddenReviewer;

    impl ReviewRunner for ForbiddenReviewer {
        fn review(
            &mut self,
            _repo_dir: &Path,
            _prompt: &str,
        ) -> Result<ReviewOutcome, MemoryError> {
            panic!("this pass must not pay for a review");
        }
    }

    /// `gh api .../protection` on a branch with no classic protection.
    fn unprotected() -> Result<GhOutput, MemoryError> {
        Ok(GhOutput {
            stdout: String::new(),
            stderr: "gh: Branch not protected (HTTP 404)".into(),
            success: false,
            code: Some(1),
        })
    }

    /// The rulesets endpoint reporting no rules in force.
    fn no_rules() -> Result<GhOutput, MemoryError> {
        ok("[[]]")
    }

    fn ok(stdout: &str) -> Result<GhOutput, MemoryError> {
        Ok(GhOutput {
            stdout: stdout.into(),
            stderr: String::new(),
            success: true,
            code: Some(0),
        })
    }

    fn issue_list_json(number: u64, labels: &[&str]) -> String {
        format!(
            r#"[{{"number":{number},"title":"t","body":"b","labels":[{}],"updatedAt":"2026-09-03T00:00:00Z"}}]"#,
            labels
                .iter()
                .map(|l| format!(r#"{{"name":"{l}"}}"#))
                .collect::<Vec<_>>()
                .join(",")
        )
    }

    fn pr_list_json(number: u64, head_sha: &str) -> String {
        format!(
            r#"[{{"number":{number},"headRefName":"{}","headRefOid":"{head_sha}","baseRefName":"main","isDraft":false,"url":"u"}}]"#,
            worktree::branch_name(&issue())
        )
    }

    /// `gh pr view` as rung 6 reads it, for a mergeable open PR.
    fn pr_view_json(head_sha: &str) -> String {
        format!(
            r#"{{"number":322,"state":"OPEN","isDraft":false,"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","baseRefName":"main","headRefName":"{}","headRefOid":"{head_sha}","reviewDecision":"APPROVED"}}"#,
            worktree::branch_name(&issue())
        )
    }

    /// A real checkout with the issue's worktree provisioned.
    fn checkout_with_worktree() -> (tempfile::TempDir, tempfile::TempDir) {
        let repo = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(repo.path())
                .output()
                .expect("git must be available");
            assert!(out.status.success(), "git {args:?}");
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "T"]);
        std::fs::write(repo.path().join("README.md"), "seed\n").unwrap();
        run(&["add", "README.md"]);
        run(&["commit", "-m", "seed"]);

        let roots = tempfile::tempdir().unwrap();
        worktree::ensure_worktree(repo.path(), roots.path(), &issue(), "HEAD").unwrap();
        (repo, roots)
    }

    // ── plan_advance: who is a candidate ────────────────────────────────

    #[test]
    fn a_succeeded_eligible_issue_in_an_approved_repo_is_a_candidate() {
        // The exact complement of the queue's `AlreadySucceeded` deferral —
        // the dead end this rung exists to turn into work.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let root = tempfile::tempdir().unwrap();
        let (candidates, skipped) = plan_advance(
            &db,
            &backlog(vec![listing(283, &["agent:ready", "risk:documentation"])]),
            &config(root.path(), root.path()),
        )
        .unwrap();

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].issue, issue());
        assert_eq!(candidates[0].risk_label.as_deref(), Some("documentation"));
        assert_eq!(candidates[0].green_commit_sha.as_deref(), Some(GREEN));
        assert!(skipped.is_empty());
    }

    #[test]
    fn an_issue_with_no_success_is_not_advanced() {
        let db = approved_db();
        let root = tempfile::tempdir().unwrap();
        let (candidates, skipped) = plan_advance(
            &db,
            &backlog(vec![listing(283, &["agent:ready"])]),
            &config(root.path(), root.path()),
        )
        .unwrap();
        assert!(candidates.is_empty());
        assert_eq!(skipped[0].reason, SkipReason::NoSuccessYet);
    }

    #[test]
    fn an_unapproved_repo_advances_nothing() {
        // The review prompt is built from the *approved* gate commands, so
        // there is nothing coherent to review without them.
        let db = Database::open_in_memory().unwrap();
        record_success(&db, &issue(), Some(GREEN));
        let root = tempfile::tempdir().unwrap();
        let (candidates, skipped) = plan_advance(
            &db,
            &backlog(vec![listing(283, &["agent:ready"])]),
            &config(root.path(), root.path()),
        )
        .unwrap();
        assert!(candidates.is_empty());
        assert_eq!(skipped[0].reason, SkipReason::RepoNotApproved);
    }

    #[test]
    fn an_issue_a_human_took_back_is_left_alone() {
        // `agent:exhausted` and `agent:blocked` are a human's stop sign, and
        // a landed PR does not override one.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let root = tempfile::tempdir().unwrap();
        for label in ["agent:exhausted", "agent:blocked"] {
            let (candidates, skipped) = plan_advance(
                &db,
                &backlog(vec![listing(283, &["agent:ready", label])]),
                &config(root.path(), root.path()),
            )
            .unwrap();
            assert!(candidates.is_empty(), "{label} must stop the advance");
            assert!(matches!(skipped[0].reason, SkipReason::NotEligible { .. }));
        }
    }

    #[test]
    fn conflicting_risk_labels_are_joined_rather_than_picked_between() {
        // Rung 8's rule, reused rather than re-derived: the joined value is
        // not a RiskClass, so it lands at `ClassMismatch` and holds.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let root = tempfile::tempdir().unwrap();
        let (candidates, _) = plan_advance(
            &db,
            &backlog(vec![listing(
                283,
                &["agent:ready", "risk:logic", "risk:documentation"],
            )]),
            &config(root.path(), root.path()),
        )
        .unwrap();
        assert_eq!(
            candidates[0].risk_label.as_deref(),
            Some("documentation+logic")
        );
        assert_eq!(
            dispatch_class(candidates[0].risk_label.as_deref()),
            "documentation+logic"
        );
    }

    #[test]
    fn an_issue_with_no_risk_label_advances_as_unclassified() {
        // Which cannot auto-merge. Requiring a human to have written
        // `risk:*` before a PR merges without one is the authorization, not
        // a gap in the automation.
        assert_eq!(dispatch_class(None), super::super::lead::UNCLASSIFIED);
    }

    // ── next_step: which step an issue is at ────────────────────────────

    #[test]
    fn a_succeeded_issue_with_no_open_pr_is_stalled_not_advanced() {
        let db = approved_db();
        let root = tempfile::tempdir().unwrap();
        let candidate = Candidate {
            issue: issue(),
            repo_path: root.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };
        let step = next_step(&db, &candidate, &PrLookup::None, root.path()).unwrap();
        assert!(matches!(step, AdvanceStep::Stalled(Stall::NoOpenPr { .. })));
    }

    #[test]
    fn two_open_prs_stall_the_issue_rather_than_merging_one_of_them() {
        let db = approved_db();
        let root = tempfile::tempdir().unwrap();
        let candidate = Candidate {
            issue: issue(),
            repo_path: root.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };
        let step = next_step(
            &db,
            &candidate,
            &PrLookup::Ambiguous {
                numbers: vec![7, 42],
            },
            root.path(),
        )
        .unwrap();
        match step {
            AdvanceStep::Stalled(Stall::AmbiguousPr { numbers }) => {
                assert_eq!(numbers, vec![7, 42])
            }
            other => panic!("expected a stall, got {other:?}"),
        }
    }

    #[test]
    fn a_pr_whose_head_has_never_been_reviewed_is_reviewed() {
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        let candidate = Candidate {
            issue: issue(),
            repo_path: repo.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };
        let step = next_step(&db, &candidate, &found(322, GREEN), roots.path()).unwrap();
        assert!(matches!(step, AdvanceStep::Review { pr_number: 322, .. }));
    }

    #[test]
    fn a_pr_already_reviewed_at_this_head_is_not_reviewed_again() {
        // The trigger is "no review has read *this commit*", so a PR whose
        // head has not moved is never re-billed. Without this the pass buys
        // a fresh review every cron tick, forever.
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        reviewed_at(&db, &issue(), 322, GREEN);

        let candidate = Candidate {
            issue: issue(),
            repo_path: repo.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };
        let step = next_step(&db, &candidate, &found(322, GREEN), roots.path()).unwrap();
        assert!(matches!(step, AdvanceStep::Merge { pr_number: 322, .. }));
    }

    #[test]
    fn a_review_of_an_older_commit_does_not_authorize_the_current_head() {
        // The same equality rung 6 enforces before merging. An IC that
        // pushed a fix is re-reviewed automatically.
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        reviewed_at(&db, &issue(), 322, GREEN);

        let candidate = Candidate {
            issue: issue(),
            repo_path: repo.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };
        let step = next_step(&db, &candidate, &found(322, MOVED), roots.path()).unwrap();
        assert!(
            matches!(step, AdvanceStep::Review { .. }),
            "a moved head must be reviewed again, got {step:?}"
        );
    }

    #[test]
    fn the_gate_is_green_only_when_the_pr_head_is_the_commit_it_was_green_at() {
        // The load-bearing guard of the rung. The gate ran at one commit; a
        // branch with commits pushed after it has unverified code at its
        // head, and `decide_merge` authorizes an auto-merge on green.
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        let candidate = Candidate {
            issue: issue(),
            repo_path: repo.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };

        let same = next_step(&db, &candidate, &found(322, GREEN), roots.path()).unwrap();
        let moved = next_step(&db, &candidate, &found(322, MOVED), roots.path()).unwrap();
        match (same, moved) {
            (
                AdvanceStep::Review {
                    gate_green: green_here,
                    ..
                },
                AdvanceStep::Review {
                    gate_green: green_there,
                    ..
                },
            ) => {
                assert!(green_here, "the gate was green at this very commit");
                assert!(
                    !green_there,
                    "a head that moved past the green commit is NOT green"
                );
            }
            other => panic!("expected two reviews, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_green_commit_is_not_green() {
        // Unknown fails closed, never toward a number — rung 5's lesson.
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        let candidate = Candidate {
            issue: issue(),
            repo_path: repo.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: None,
        };
        match next_step(&db, &candidate, &found(322, GREEN), roots.path()).unwrap() {
            AdvanceStep::Review { gate_green, .. } => assert!(!gate_green),
            other => panic!("expected a review, got {other:?}"),
        }
    }

    #[test]
    fn a_missing_worktree_stalls_the_review_rather_than_reviewing_the_wrong_checkout() {
        // A reviewer reads the diff from the checkout it is pointed at. One
        // that does not contain the branch does not fail — it writes a
        // confident review of something else, and that review authorizes a
        // merge.
        let db = approved_db();
        let repo = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let candidate = Candidate {
            issue: issue(),
            repo_path: repo.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };
        let step = next_step(&db, &candidate, &found(322, GREEN), roots.path()).unwrap();
        assert!(
            matches!(step, AdvanceStep::Stalled(Stall::WorktreeMissing { .. })),
            "got {step:?}"
        );
    }

    #[test]
    fn a_missing_worktree_does_not_stall_a_pr_that_is_already_reviewed() {
        // The merge step is pure `gh` and needs no checkout at all.
        let db = approved_db();
        reviewed_at(&db, &issue(), 322, GREEN);
        let repo = tempfile::tempdir().unwrap();
        let roots = tempfile::tempdir().unwrap();
        let candidate = Candidate {
            issue: issue(),
            repo_path: repo.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };
        assert!(matches!(
            next_step(&db, &candidate, &found(322, GREEN), roots.path()).unwrap(),
            AdvanceStep::Merge { .. }
        ));
    }

    // ── advance_pass: the pass itself ───────────────────────────────────

    #[test]
    fn without_the_merge_flag_every_merge_is_rehearsed() {
        // Merge is the only irreversible action in the subsystem. Rung 9's
        // precedent: a switch deciding whether a new kind of irreversible
        // action happens at all is the operator's explicit choice.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            unprotected(),
            no_rules(),
        ]);
        let mut reviewer = StubReviewer::passing();

        let report = advance_pass(
            &db,
            &mut gh,
            &mut reviewer,
            &config(roots.path(), repo.path()),
        )
        .unwrap();

        assert!(!report.merge_enabled);
        assert_eq!(reviewer.calls, 1, "reviewing is not gated on --merge");
        assert!(
            !gh.seen
                .iter()
                .any(|argv| argv.contains(&"merge".to_string())),
            "no `gh pr merge` was run: {:?}",
            gh.seen
        );
        let advanced = &report.advanced[0];
        assert!(matches!(
            advanced.merge.as_ref().unwrap().outcome,
            merge::MergeOutcome::WouldMerge { .. }
        ));
        assert!(advanced.cleanup.is_none(), "a rehearsal cleans nothing up");
    }

    #[test]
    fn a_dry_run_spends_nothing_and_reviews_nothing() {
        // A review is a paid call and a written drawer. "Change nothing"
        // has to include it.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.dry_run = true;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        assert!(report.dry_run);
        assert!(report.advanced[0].review.is_none());
        assert!(report.advanced[0].merge.is_none());
    }

    #[test]
    fn a_landed_merge_gives_the_worktree_and_the_slot_back() {
        // The step nothing did before this rung: the spec's "records
        // outcome, cleans worktree".
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        // A dispatch-state drawer left behind by a paused run. It holds a
        // concurrency slot against work that has now landed.
        dispatch_state::upsert_dispatch_state(
            &db,
            &DispatchState {
                issue: issue(),
                worktree_path: worktree::worktree_path(roots.path(), &issue())
                    .to_string_lossy()
                    .to_string(),
                ic_session_name: "autopilot-ic-283".into(),
                dispatch_class: "documentation".into(),
                attempt_n: 1,
                state: "paused-daily-budget".into(),
                started_at: "2026-09-03T00:00:00Z".into(),
                session_uuid: "11111111-2222-3333-4444-555555555555".into(),
                turn_n: 1,
                session_claimed: true,
            },
        )
        .unwrap();

        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            // Neither classic protection nor a ruleset stands in the way, so
            // rung 6 is permitted to merge.
            unprotected(),
            no_rules(),
            ok("merged"),
            // clearing the `agent:*` labels: read, then edit
            ok(r#"{"labels":[{"name":"agent:ready"}]}"#),
            ok(""),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.merge = true;

        let report = advance_pass(&db, &mut gh, &mut StubReviewer::passing(), &config).unwrap();
        let advanced = &report.advanced[0];

        let outcome = &advanced.merge.as_ref().unwrap().outcome;
        assert!(outcome.landed(), "the PR must land, got {outcome:?}");

        let cleanup = advanced
            .cleanup
            .as_ref()
            .expect("a landed PR is cleaned up");
        assert!(cleanup.dispatch_state_cleared, "the slot is given back");
        assert!(
            matches!(cleanup.worktree, WorktreeRemoval::Removed { .. }),
            "the worktree is given back, got {:?}",
            cleanup.worktree
        );
        assert!(cleanup.error.is_none(), "{:?}", cleanup.error);
        assert!(
            dispatch_state::get_dispatch_state(&db, &issue())
                .unwrap()
                .is_none(),
            "the concurrency slot a paused run held is released"
        );
        assert!(
            !worktree::worktree_path(roots.path(), &issue()).exists(),
            "the checkout is gone from disk"
        );
    }

    // ── rung 11: the red path ───────────────────────────────────────────

    /// A reviewer that asks for changes, with a reason.
    fn needs_changes() -> StubReviewer {
        StubReviewer {
            calls: 0,
            outcome: ReviewOutcome {
                verdict: Some(ReviewVerdict::NeedsChanges),
                risk_class: Some(RiskClass::Documentation),
                reason: Some("the retry loop is unbounded".to_string()),
                total_cost_usd: None,
                token_usage: None,
                process_success: true,
            },
        }
    }

    #[test]
    fn a_needs_changes_verdict_arms_a_re_dispatch_instead_of_blocking_the_issue() {
        // Rung 11's central path, and the reason the merge below it must be
        // rehearsed: rung 6's real `NeedsChanges` hold sets `agent:blocked`,
        // which strips `agent:ready` and drops the issue out of the Lead's
        // listing — so the re-dispatch just armed would never be dispatched.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.merge = true;
        config.remediate = true;

        let report = advance_pass(&db, &mut gh, &mut needs_changes(), &config).unwrap();
        let advanced = &report.advanced[0];

        assert!(
            matches!(
                advanced.remediation,
                Some(ArmOutcome::Armed {
                    pr_number: 322,
                    has_findings: true,
                    ..
                })
            ),
            "got {:?}",
            advanced.remediation
        );
        assert!(
            matches!(
                advanced.merge.as_ref().unwrap().outcome,
                MergeOutcome::Held(_)
            ),
            "the PR is still not merged with an unresolved finding"
        );
        let execution = advanced.merge.as_ref().unwrap();
        assert!(
            execution.label_plan.is_none() && !execution.commented,
            "an armed remediation forces the rehearsal: `agent:ready` survives \
and no `agent:blocked` comment is posted, or the Lead never sees the issue again"
        );

        // And the findings actually reached storage, where the next dispatch
        // reads them from.
        let armed = remediate::active_remediation(&db, &issue())
            .unwrap()
            .unwrap();
        assert_eq!(
            armed.findings.as_deref(),
            Some("the retry loop is unbounded")
        );
        assert_eq!(armed.head_sha, GREEN);
    }

    #[test]
    fn without_remediate_a_needs_changes_verdict_is_held_for_a_human() {
        // Rung 10's behaviour, byte for byte. Lesson 43: a new optional
        // feature must not change the configuration that predates it.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            ok(r#"{"labels":[{"name":"agent:ready"}]}"#),
            ok(""),
            ok(""),
            ok(""),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.merge = true;

        let report = advance_pass(&db, &mut gh, &mut needs_changes(), &config).unwrap();
        let advanced = &report.advanced[0];
        assert_eq!(advanced.remediation, None);
        let execution = advanced.merge.as_ref().unwrap();
        assert!(
            execution.label_plan.is_some() && execution.commented,
            "without --remediate the hold runs for real, exactly as rung 10 shipped it"
        );
        assert_eq!(remediate::get_remediation(&db, &issue()).unwrap(), None);
    }

    #[test]
    fn an_issue_at_its_attempt_cap_falls_through_to_the_human_hold() {
        // The spec's "on exhaustion the PR stays open for a human". The
        // automation has given up, so the merge runs for real and rung 6
        // comments and flips the label — which is the notification.
        let db = approved_db();
        lineage::upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: Some(GREEN.to_string()),
                cumulative_attempt_n: 5,
            },
        )
        .unwrap();
        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            ok(r#"{"labels":[{"name":"agent:ready"}]}"#),
            ok(""),
            ok(""),
            ok(""),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.merge = true;
        config.remediate = true;
        config.attempt_cap = 5;

        let report = advance_pass(&db, &mut gh, &mut needs_changes(), &config).unwrap();
        let advanced = &report.advanced[0];
        assert!(
            matches!(
                advanced.remediation,
                Some(ArmOutcome::CapReached {
                    cumulative_attempt_n: 5,
                    attempt_cap: 5
                })
            ),
            "got {:?}",
            advanced.remediation
        );
        let execution = advanced.merge.as_ref().unwrap();
        assert!(
            execution.commented,
            "an exhausted issue must reach the real hold, or nobody is told"
        );
        assert_eq!(
            remediate::get_remediation(&db, &issue()).unwrap(),
            None,
            "nothing is armed for an issue that can never be dispatched"
        );
    }

    #[test]
    fn the_cap_is_reached_while_a_remediation_is_still_in_force_and_the_human_is_told() {
        // The ordinary way an unproductive remediation ends, and the one the
        // record itself can hide. The issue has spent its attempts, so nothing
        // will re-dispatch it — but its remediation record is still *active*
        // (no newer success has landed), so a `remediation_in_force` derived
        // from `active_remediation` alone would rehearse the merge for ever
        // and rung 6's hold would never comment or label. `CapReached` is not
        // `in_force`, and that is what keeps the human reachable.
        //
        // `an_issue_at_its_attempt_cap_falls_through_to_the_human_hold` does
        // not cover this: it has no remediation record at all, so both
        // spellings agree there.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        reviewed_at_needing_changes(&db, &issue(), 322, GREEN);
        remediate::arm_remediation(
            &db,
            &ArmRequest {
                issue: &issue(),
                pr_number: 322,
                head_sha: GREEN,
                findings: Some("the retry loop is unbounded"),
                attempt_cap: 5,
            },
        )
        .unwrap();
        // The remediation dispatches spent the cap and never fixed it. Same
        // commit, so the remediation is still in force.
        lineage::upsert_issue_status(
            &db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: Some(GREEN.to_string()),
                cumulative_attempt_n: 5,
            },
        )
        .unwrap();
        assert!(
            remediate::active_remediation(&db, &issue())
                .unwrap()
                .is_some(),
            "test setup: the record must still be active, or this proves nothing"
        );

        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            ok(r#"{"labels":[{"name":"agent:ready"}]}"#),
            ok(""),
            ok(""),
            ok(""),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.merge = true;
        config.remediate = true;
        config.attempt_cap = 5;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        let advanced = &report.advanced[0];
        assert!(
            matches!(advanced.remediation, Some(ArmOutcome::CapReached { .. })),
            "got {:?}",
            advanced.remediation
        );
        assert!(
            advanced.merge.as_ref().unwrap().commented,
            "an exhausted issue reaches rung 6's real hold even with an active \
remediation record — otherwise the PR sits open and nobody is ever told"
        );
    }

    #[test]
    fn a_needs_changes_about_a_commit_that_is_no_longer_the_head_arms_nothing() {
        // A verdict objects to the commit it read, and this is the case where
        // that matters: two `needs_changes` reviews, then a force-push back
        // to the earlier commit. The head is at GREEN, which review 1 did
        // read — but the *latest* review is about MOVED, which is no longer
        // there. Arming from it would hand the IC findings about a diff that
        // does not exist on the branch.
        //
        // The latest review is the one consulted, not the latest matching
        // one: rung 6's rule, for its reason — looking further back to find a
        // review that fits is how a re-review's verdict gets ignored.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        reviewed_at_needing_changes(&db, &issue(), 322, GREEN);
        reviewed_at_needing_changes(&db, &issue(), 322, MOVED);

        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.remediate = true;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        assert_eq!(
            report.advanced[0].remediation, None,
            "findings about a commit that is not the head must not arm a fix"
        );
        assert_eq!(remediate::get_remediation(&db, &issue()).unwrap(), None);
    }

    #[test]
    fn a_passing_review_at_the_head_arms_nothing() {
        // The complement: only `needs_changes` opens the red path.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        reviewed_at_needing_changes(&db, &issue(), 322, MOVED);
        reviewed_at(&db, &issue(), 322, GREEN);

        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            unprotected(),
            no_rules(),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.remediate = true;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        assert_eq!(report.advanced[0].remediation, None);
    }

    #[test]
    fn a_dry_run_arms_nothing() {
        // `--dry-run` means "read everything, change nothing", and an armed
        // remediation is a change that costs money on the next tick.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        reviewed_at(&db, &issue(), 322, GREEN);
        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            unprotected(),
            no_rules(),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.remediate = true;
        config.dry_run = true;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        assert!(!report.remediate_enabled);
        assert_eq!(remediate::get_remediation(&db, &issue()).unwrap(), None);
    }

    #[test]
    fn a_second_pass_over_an_armed_issue_reports_it_and_rewrites_nothing() {
        // Idempotence keyed on (pr, commit), including the failure outcomes:
        // a remediation that was armed and dispatched and did not work is the
        // same remediation, not a new one. Rewriting it would reset the
        // delivery depth and make "has this been dispatched?" unanswerable.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        reviewed_at_needing_changes(&db, &issue(), 322, GREEN);
        let (repo, roots) = checkout_with_worktree();

        let mut config = config(roots.path(), repo.path());
        config.remediate = true;

        let script = || {
            ScriptedGh::new(vec![
                ok(&issue_list_json(
                    283,
                    &["agent:ready", "risk:documentation"],
                )),
                ok(&pr_list_json(322, GREEN)),
                ok(&pr_view_json(GREEN)),
            ])
        };

        let first = advance_pass(&db, &mut script(), &mut ForbiddenReviewer, &config).unwrap();
        assert!(matches!(
            first.advanced[0].remediation,
            Some(ArmOutcome::Armed { .. })
        ));

        let second = advance_pass(&db, &mut script(), &mut ForbiddenReviewer, &config).unwrap();
        assert!(
            matches!(
                second.advanced[0].remediation,
                Some(ArmOutcome::AlreadyArmed {
                    dispatches_since: 0,
                    ..
                })
            ),
            "got {:?}",
            second.advanced[0].remediation
        );
    }

    #[test]
    fn a_superseded_remediation_stops_rehearsing_and_reaches_the_human_hold() {
        // `ArmOutcome::AlreadyArmed` and `active_remediation` answer different
        // questions, and this is the state where they disagree: the IC
        // committed locally and never pushed, so `best_commit_sha` moved to
        // MOVED while the PR's head stayed at GREEN, where the reviewer left
        // it. The Lead reads `active_remediation`, sees a newer success, and
        // defers `AlreadySucceeded` — nothing will ever re-dispatch this
        // issue. Rehearsing the merge on `AlreadyArmed` alone would then hold
        // the PR open, un-commented and unlabelled, on every pass for ever,
        // and `took_a_step` would charge nothing for the repetition.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        reviewed_at_needing_changes(&db, &issue(), 322, GREEN);
        remediate::arm_remediation(
            &db,
            &ArmRequest {
                issue: &issue(),
                pr_number: 322,
                head_sha: GREEN,
                findings: Some("the retry loop is unbounded"),
                attempt_cap: crate::autopilot::run::DEFAULT_ATTEMPT_CAP,
            },
        )
        .unwrap();
        // The IC's fix never reached the remote: a newer success, the same PR
        // head.
        record_success(&db, &issue(), Some(MOVED));
        assert_eq!(
            remediate::active_remediation(&db, &issue()).unwrap(),
            None,
            "test setup: the remediation must already be superseded"
        );

        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            ok(r#"{"labels":[{"name":"agent:ready"}]}"#),
            ok(""),
            ok(""),
            ok(""),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.merge = true;
        config.remediate = true;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        let execution = report.advanced[0].merge.as_ref().unwrap();
        assert!(
            execution.commented,
            "a remediation nothing will dispatch must fall through to the real \
hold, or the PR is stranded with nobody told"
        );
    }

    const TWO_READY: &str = r#"[{"number":283,"title":"a","body":"b","labels":[{"name":"agent:ready"},{"name":"risk:documentation"}],"updatedAt":"2026-09-03T00:00:00Z"},{"number":284,"title":"a","body":"b","labels":[{"name":"agent:ready"},{"name":"risk:documentation"}],"updatedAt":"2026-09-03T00:00:00Z"}]"#;

    #[test]
    fn an_already_armed_remediation_does_not_spend_the_burst_limit() {
        // Rung 10's lesson 52 in its transient form. An IC can take several
        // dispatches to push a fix; while it does, the issue is re-reported
        // on every pass with no review bought and no merge written. Charging
        // those reports against the limit would let three remediating issues
        // fill every pass and starve the green PRs behind them.
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        let mut config = config(roots.path(), repo.path());
        config.remediate = true;
        config.max_advances_per_pass = 1;

        // Issue 283 is mid-remediation; issue 284 is green and ready to merge.
        record_success(&db, &issue(), Some(GREEN));
        reviewed_at_needing_changes(&db, &issue(), 322, GREEN);
        remediate::arm_remediation(
            &db,
            &remediate::ArmRequest {
                issue: &issue(),
                pr_number: 322,
                head_sha: GREEN,
                findings: Some("the retry loop is unbounded"),
                attempt_cap: config.attempt_cap,
            },
        )
        .unwrap();

        let second = IssueRef::new(REPO, 284);
        record_success(&db, &second, Some(GREEN));
        reviewed_at(&db, &second, 323, GREEN);

        let mut gh = ScriptedGh::new(vec![
            ok(TWO_READY),
            // 283: already armed, nothing bought.
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            // 284 still gets its turn.
            ok(&pr_list_json(323, GREEN)),
            ok(&pr_view_json(GREEN)),
            unprotected(),
            no_rules(),
        ]);

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        assert!(
            matches!(
                report.advanced[0].remediation,
                Some(ArmOutcome::AlreadyArmed { .. })
            ),
            "got {:?}",
            report.advanced[0].remediation
        );
        assert_eq!(
            report.advanced.len(),
            2,
            "the green PR behind the remediating issue must still be reached, \
skipped: {:?}",
            report.skipped
        );
        assert!(
            report
                .skipped
                .iter()
                .all(|s| !matches!(s.reason, SkipReason::PassLimitReached { .. })),
            "nothing was deferred on the burst limit: {:?}",
            report.skipped
        );
    }

    #[test]
    fn a_held_merge_cleans_nothing_up() {
        // The other half of the rule: cleanup is terminal bookkeeping for a
        // PR that landed, and a held PR still needs its worktree — the next
        // pass reviews the fix in it.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
            // A draft PR: rung 6 holds, whatever the review said.
            ok(&pr_view_json(GREEN).replace(r#""isDraft":false"#, r#""isDraft":true"#)),
            ok(r#"{"labels":[{"name":"agent:ready"}]}"#),
            ok(""),
            ok(""),
            ok(""),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.merge = true;

        let report = advance_pass(&db, &mut gh, &mut StubReviewer::passing(), &config).unwrap();
        let advanced = &report.advanced[0];
        assert!(!advanced.merge.as_ref().unwrap().outcome.landed());
        assert!(advanced.cleanup.is_none());
        assert!(
            worktree::worktree_path(roots.path(), &issue()).exists(),
            "a held PR keeps the checkout the next review reads"
        );
    }

    #[test]
    fn a_repo_whose_issues_cannot_be_listed_is_a_problem_not_an_empty_backlog() {
        // Rung 7's lesson 21 at repo granularity. An empty listing would be
        // a confident claim that a repo has no landed work.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput {
            stdout: String::new(),
            stderr: "could not reach api.github.com".into(),
            success: false,
            code: Some(1),
        })]);

        let report = advance_pass(
            &db,
            &mut gh,
            &mut ForbiddenReviewer,
            &config(roots.path(), repo.path()),
        )
        .unwrap();
        assert!(report.advanced.is_empty());
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].what.contains("list"));
    }

    #[test]
    fn one_issues_failure_does_not_strand_the_next_issues_pr() {
        // The whole point of a pass that runs unattended.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        record_success(&db, &IssueRef::new(REPO, 284), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        worktree::ensure_worktree(repo.path(), roots.path(), &IssueRef::new(REPO, 284), "HEAD")
            .unwrap();

        let mut gh = ScriptedGh::new(vec![
            ok(
                r#"[{"number":283,"title":"a","body":"b","labels":[{"name":"agent:ready"}],"updatedAt":"2026-09-03T00:00:00Z"},{"number":284,"title":"c","body":"d","labels":[{"name":"agent:ready"}],"updatedAt":"2026-09-03T00:00:00Z"}]"#,
            ),
            // Issue 283's PR lookup fails outright.
            Ok(GhOutput {
                stdout: String::new(),
                stderr: "boom".into(),
                success: false,
                code: Some(1),
            }),
            // Issue 284's lookup answers: no open PR.
            ok("[]"),
        ]);

        let report = advance_pass(
            &db,
            &mut gh,
            &mut ForbiddenReviewer,
            &config(roots.path(), repo.path()),
        )
        .unwrap();
        assert_eq!(report.problems.len(), 1, "283 failed");
        assert_eq!(report.advanced.len(), 1, "284 was still looked at");
        assert_eq!(report.advanced[0].issue.number, 284);
    }

    /// The three-issue `agent:ready` listing both burst-limit tests read.
    const THREE_READY: &str = r#"[{"number":283,"title":"a","body":"b","labels":[{"name":"agent:ready"}],"updatedAt":"2026-09-03T00:00:00Z"},{"number":284,"title":"a","body":"b","labels":[{"name":"agent:ready"}],"updatedAt":"2026-09-03T00:00:00Z"},{"number":285,"title":"a","body":"b","labels":[{"name":"agent:ready"}],"updatedAt":"2026-09-03T00:00:00Z"}]"#;

    #[test]
    fn the_pass_limit_bounds_the_burst_and_names_what_it_deferred() {
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        for n in [283u64, 284, 285] {
            record_success(&db, &IssueRef::new(REPO, n), Some(GREEN));
            reviewed_at(&db, &IssueRef::new(REPO, n), 322, GREEN);
        }
        let mut gh = ScriptedGh::new(vec![
            ok(THREE_READY),
            // Only issue 283 gets this far: one PR lookup, one snapshot and
            // the two protection reads. A fourth `gh` call would mean the
            // limit did not bind, and `ScriptedGh` panics on one.
            ok(&pr_list_json(322, GREEN)),
            ok(&pr_view_json(GREEN)),
            unprotected(),
            no_rules(),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.max_advances_per_pass = 1;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        assert_eq!(report.advanced.len(), 1);
        assert_eq!(report.advanced[0].issue.number, 283);
        assert_eq!(
            report
                .skipped
                .iter()
                .filter(|s| matches!(s.reason, SkipReason::PassLimitReached { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn a_stall_does_not_spend_the_burst_limit_it_took_no_step_with() {
        // The starvation this counter used to cause. A stall is a fact only a
        // human can change — an issue left `agent:ready` after its PR was
        // merged and closed is the ordinary case — so it is reported again on
        // every pass, forever. Charging those repeats against the limit let
        // three of them fill it permanently, and every green PR behind them
        // was then never reviewed and never merged.
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        for n in [283u64, 284, 285] {
            record_success(&db, &IssueRef::new(REPO, n), Some(GREEN));
        }
        let mut gh = ScriptedGh::new(vec![ok(THREE_READY), ok("[]"), ok("[]"), ok("[]")]);
        let mut config = config(roots.path(), repo.path());
        config.max_advances_per_pass = 1;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        assert_eq!(report.advanced.len(), 3, "all three stalls are reported");
        assert!(report
            .advanced
            .iter()
            .all(|a| matches!(a.step, AdvanceStep::Stalled(Stall::NoOpenPr { .. }))));
        assert!(
            !report
                .skipped
                .iter()
                .any(|s| matches!(s.reason, SkipReason::PassLimitReached { .. })),
            "a stall took no step, so it spent none of the burst limit"
        );
    }

    #[test]
    fn a_refused_review_stops_the_issue_rather_than_blocking_it_as_unreviewed() {
        // `review_pr` records nothing when the day's ceilings refuse it, so
        // falling through to `execute_merge` would hold `NotReviewed`, comment,
        // and move the issue to `agent:blocked` — which strips `agent:ready`
        // and never self-resumes, because a merge hold carries no question
        // marker. A budget that rolls over must not need a human to unpick
        // every green issue.
        let db = approved_db();
        record_success(&db, &issue(), Some(GREEN));
        let (repo, roots) = checkout_with_worktree();
        // Spend the day's dollars, which is what `advance` inherits from the
        // night's dispatches.
        super::super::budget::accumulate_daily_spend(&db, &super::super::today_utc(), 40.0)
            .unwrap();

        let mut gh = ScriptedGh::new(vec![
            ok(&issue_list_json(
                283,
                &["agent:ready", "risk:documentation"],
            )),
            ok(&pr_list_json(322, GREEN)),
        ]);
        let mut config = config(roots.path(), repo.path());
        config.merge = true;

        let report = advance_pass(&db, &mut gh, &mut ForbiddenReviewer, &config).unwrap();
        let advanced = &report.advanced[0];
        assert_eq!(
            advanced.review.as_ref().unwrap().refusal,
            Some(review::ReviewRefusal::DailyBudgetExhausted)
        );
        assert!(
            advanced.merge.is_none(),
            "a refused review must not reach the merge"
        );
        assert!(
            !gh.seen
                .iter()
                .any(|argv| argv.contains(&"edit".to_string())),
            "no label was written: {:?}",
            gh.seen
        );
    }

    #[test]
    fn a_retargeted_pr_is_reviewed_again_rather_than_held_forever() {
        // Rung 6 refuses to merge a PR whose base moved since the review. The
        // head never moves when a PR is retargeted, so a head-only trigger
        // reported it as already reviewed on every pass and it sat at
        // `BaseBranchMismatch` with no way out.
        let db = approved_db();
        let (repo, roots) = checkout_with_worktree();
        reviewed_at(&db, &issue(), 322, GREEN);

        let candidate = Candidate {
            issue: issue(),
            repo_path: repo.path().to_path_buf(),
            risk_label: None,
            green_commit_sha: Some(GREEN.into()),
        };
        let mut retargeted = found(322, GREEN);
        if let PrLookup::Found(pr) = &mut retargeted {
            pr.base_branch = "release/1.x".to_string();
        }
        let step = next_step(&db, &candidate, &retargeted, roots.path()).unwrap();
        assert!(
            matches!(step, AdvanceStep::Review { .. }),
            "a moved base must be reviewed again, got {step:?}"
        );
    }

    #[test]
    fn a_nan_budget_is_refused_rather_than_silently_removing_the_ceiling() {
        let root = tempfile::tempdir().unwrap();
        let mut config = config(root.path(), root.path());
        config.daily_budget_usd = f64::NAN;
        assert!(config.validate().is_err());
        config.daily_budget_usd = 25.0;
        config.max_advances_per_pass = 0;
        assert!(config.validate().is_err());
    }
}
