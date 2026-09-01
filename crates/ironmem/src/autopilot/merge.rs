//! Merge authority and cross-dispatch stagnation — build-ladder rung 6.
//!
//! Rung 5 answered *"may this merge?"* and stopped there deliberately:
//! [`super::review::decide_merge`] returns a [`MergeDecision`] and executes
//! nothing. This module is that decision's only consumer, and it does the
//! three things the spec's data flow puts after the review:
//!
//! - **`PASS` + low-risk + class match ─► `Lead merges via gh pr merge`.**
//! - **otherwise ─► `PR stays open, labeled, human notified`.**
//! - **retries exhausted ─► `agent:exhausted`**, with a comment summarizing
//!   everything that was tried.
//!
//! # The decision is re-derived, not replayed
//!
//! [`execute_merge`] reads the issue's recorded reviews and calls
//! [`super::review::decide_merge`] again on the facts it finds, rather than
//! deserializing the decision rung 5 already stored. Two reasons, and the
//! second is the load-bearing one:
//!
//! 1. There is then exactly one implementation of the merge guard in the
//!    codebase. A stored decision would be a second answer that could drift.
//! 2. `gate_green` is an input to the decision and is a *present-tense* fact.
//!    A `PASS` recorded when the gate was green says nothing about a gate
//!    that has since gone red.
//!
//! # Five more guards, all execution-time
//!
//! Rung 5's guards are about the review. These are about the world between
//! the review and the merge, and none of them can be expressed in rung 5's
//! vocabulary because rung 5 never talks to GitHub:
//!
//! | Guard | Why it cannot be skipped |
//! |---|---|
//! | A review exists for **this PR** | A `PASS` on PR #10 is not a `PASS` on PR #12 |
//! | Its head SHA is the PR's head SHA | Otherwise the merge lands a commit no reviewer read |
//! | The PR is open, not a draft, mergeable | GitHub would refuse anyway; refusing first yields a reason a human can act on |
//! | The base branch is the one that was reviewed | A retargeted PR was reviewed against a different diff |
//! | The base branch's review requirement is met | The spec's `enforce_admins` case — see below |
//!
//! # Branch protection: the open question rung 6 was handed
//!
//! The design names the Lead as "SOLE merge authority via `gh pr merge`", and
//! the ladder's notes flag that repos whose default branch requires a
//! human-approved review — *this* repository among them, with
//! `enforce_admins` on and therefore no admin bypass — make that claim false.
//! Resolved by asking rather than assuming, in two halves that have to be
//! asked together:
//!
//! 1. **Does the base branch require a human approval?** Read from the
//!    branch's protection rules *and* from the rulesets in force on it — the
//!    modern mechanism, which the classic protection endpoint reports as a
//!    404 and this once read as "unprotected".
//! 2. **Does this PR have one?** Read from GitHub's own `reviewDecision`
//!    for the PR. Protection describes the branch, not the pull request: it
//!    still says an approving review is required after one has been given,
//!    so answering only the first question refused an approved PR forever.
//!
//! A requirement that is unmet holds with
//! [`MergeHold::HumanApprovalRequired`]. Autopilot is not the reviewer of
//! record there and cannot become one; the honest outcome is a labeled PR
//! and a notified human, which is the same terminal state the spec already
//! defines for every non-low-risk change. A requirement that is *met* is not
//! a hold — the human supplied exactly what the rule asked for.
//!
//! The guard runs before the mergeability check, because GitHub reports
//! `mergeStateStatus: BLOCKED` for a PR whose only problem is the missing
//! approval: asking about mergeability first made this guard unreachable in
//! the one configuration it exists for, and handed the human a symptom
//! instead of a cause.
//!
//! Protection that *cannot be read* holds too — see
//! [`super::gh::BranchProtection::Unknown`].

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::gh::{self, BranchProtection, GhRunner, MergeStrategy, PrSnapshot};
use super::labels::{self, AgentLabel};
use super::lineage::MAX_LINEAGE_FIELD_CHARS;
use super::review::{decide_merge, MergeDecision, RecordedReviewSummary, ReviewJudgment};
use super::scrub::scrub_and_bound;
use super::{
    validate_repo, zero_embedding, IssueRef, ADDED_BY, ISSUE_ENTITY_TYPE, MAX_ISSUE_EDGES, ROOM,
    WING,
};
use crate::db::knowledge_graph::KnowledgeGraph;
use crate::db::schema::Database;
use crate::error::MemoryError;

/// The most characters an issue comment this module posts may contain.
///
/// GitHub's own limit is 65,536; this is deliberately well under it so that
/// the bound is ours and is hit predictably, rather than GitHub's and hit as
/// a failed API call at the worst possible moment. An exhaustion summary that
/// needs more than this has more attempts than a human will read anyway, and
/// the full record is in lineage.
pub const MAX_COMMENT_CHARS: usize = 20_000;

// ── holds ───────────────────────────────────────────────────────────────

/// Why a merge did not happen, at execution time.
///
/// Distinct from [`super::review::HoldReason`], which is about the *review*.
/// The two are composed rather than merged: [`MergeHold::Review`] carries
/// rung 5's answer verbatim, so a human reading a hold can always tell
/// whether the change was judged unfit or the world simply moved.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "hold", rename_all = "snake_case")]
pub enum MergeHold {
    /// Rung 5's fail-closed gate said no.
    Review(super::review::HoldReason),
    /// No review has ever been recorded for this issue *and this PR*.
    /// "Infrastructure failure never becomes implicit approval" — and neither
    /// does never having asked.
    NotReviewed,
    /// A review exists but does not record which commit it read, so it cannot
    /// be tied to the commit being merged. Every review recorded before rung
    /// 6 is in this state.
    ReviewHeadUnknown { reviewed_at: String },
    /// The PR's head has moved since the review. The IC pushed again, or
    /// somebody did.
    ReviewIsStale {
        reviewed_head: String,
        current_head: String,
    },
    /// The PR is neither open nor merged — closed without merging, most
    /// likely. A merged PR is [`MergeOutcome::AlreadyMerged`], not a hold.
    PrNotOpen { state: String },
    /// The PR is a draft. A draft is an explicit "not yet" from whoever set
    /// it, including the IC.
    PrIsDraft,
    /// GitHub does not consider the PR mergeable, or has not finished
    /// deciding. `UNKNOWN` is held on for the same reason `Unknown`
    /// protection is: not-yet-computed is not "yes".
    PrNotMergeable {
        mergeable: String,
        merge_state_status: String,
    },
    /// The PR now targets a different base branch than the one reviewed
    /// against, so the reviewed diff is not the diff that would land.
    BaseBranchMismatch {
        reviewed_base: String,
        current_base: String,
    },
    /// The base branch requires an approving human review, and this PR does
    /// not have one.
    HumanApprovalRequired {
        base: String,
        required_approving_review_count: u64,
        /// A CODEOWNERS approval is required, independent of the count.
        require_code_owner_reviews: bool,
        enforce_admins: bool,
        /// GitHub's `reviewDecision` for this PR at the time of the hold —
        /// `REVIEW_REQUIRED`, `CHANGES_REQUESTED`, or empty. Carried so the
        /// comment can tell a human whether nobody has reviewed yet or
        /// somebody has asked for changes, which are different next steps,
        /// and so that a move between them counts as a *changed* hold and is
        /// commented on again rather than deduplicated away.
        review_decision: String,
    },
    /// The base branch's protection rules could not be read.
    ProtectionUnknown { detail: String },
    /// `gh pr merge` itself refused, and a re-read confirmed the PR is still
    /// open — so the merge definitely did not land.
    MergeCommandFailed { detail: String },
    /// `gh pr merge` failed *and* the PR's state could not be re-read, so
    /// whether the merge landed is genuinely unknown.
    ///
    /// Its own variant because the honest thing to say is the whole point.
    /// `gh pr merge` deletes the head branch *after* performing the merge,
    /// so a failure there can sit on top of a completed merge; the re-read
    /// exists to tell those apart, and when the re-read itself fails —
    /// a rate limit, a dropped socket, a `gh` that is suddenly gone — the
    /// module knows nothing. Reporting `MergeCommandFailed` there told a
    /// human "PR #N was not merged" about a PR that may well be merged,
    /// which is the fail-closed rule inverted: an unanswered question became
    /// a definite negative claim.
    MergeResultUnknown {
        detail: String,
        read_failure: String,
    },
}

impl MergeHold {
    /// One line, for a comment and for the CLI's text output.
    pub fn summary(&self) -> String {
        match self {
            // Delegated, not `{:?}`: two of `HoldReason`'s variants carry
            // payloads, and Debug-formatting them put
            // `ClassMismatch { dispatch_class: "documentation", .. }` into a
            // comment written for a human.
            MergeHold::Review(reason) => reason.summary(),
            MergeHold::NotReviewed => {
                "no review has been recorded for this PR, and gates alone never authorize a merge"
                    .to_string()
            }
            MergeHold::ReviewHeadUnknown { reviewed_at } => format!(
                "the review recorded at {reviewed_at} does not name the commit it read, \
so it cannot be tied to this PR's head"
            ),
            MergeHold::ReviewIsStale {
                reviewed_head,
                current_head,
            } => format!(
                "the PR head moved after the review: reviewed {}, now {}",
                short_sha(reviewed_head),
                short_sha(current_head)
            ),
            MergeHold::PrNotOpen { state } => format!("the PR is {state}, not open"),
            MergeHold::PrIsDraft => "the PR is a draft".to_string(),
            MergeHold::PrNotMergeable {
                mergeable,
                merge_state_status,
            } => format!(
                "GitHub reports mergeable={mergeable}, mergeStateStatus={merge_state_status}"
            ),
            MergeHold::BaseBranchMismatch {
                reviewed_base,
                current_base,
            } => format!(
                "the PR was reviewed against {reviewed_base} but now targets {current_base}"
            ),
            MergeHold::HumanApprovalRequired {
                base,
                required_approving_review_count,
                require_code_owner_reviews,
                enforce_admins,
                review_decision,
            } => {
                // Rendered from whichever requirement actually applies. A
                // count of zero with code owners required is a real
                // configuration, and "requires 0 approving review(s)" would
                // read to a human as though nothing were wrong.
                let requirement = match (
                    *required_approving_review_count,
                    *require_code_owner_reviews,
                ) {
                    (0, _) => "an approving review from a code owner".to_string(),
                    (n, false) => format!("{n} approving review(s)"),
                    (n, true) => format!("{n} approving review(s), including a code owner's"),
                };
                format!(
                    "{base} requires {requirement}{}, which Autopilot cannot supply{}",
                    if *enforce_admins {
                        " and enforces the rule on administrators too"
                    } else {
                        ""
                    },
                    if review_decision.is_empty() {
                        String::new()
                    } else {
                        format!(" (GitHub currently reports {review_decision})")
                    }
                )
            }
            MergeHold::ProtectionUnknown { detail } => {
                format!("the base branch's protection rules could not be read: {detail}")
            }
            MergeHold::MergeCommandFailed { detail } => format!("`gh pr merge` failed: {detail}"),
            MergeHold::MergeResultUnknown {
                detail,
                read_failure,
            } => format!(
                "`gh pr merge` failed ({detail}) and the pull request's state could not be \
re-read afterwards ({read_failure}), so whether the merge landed is unknown — check the \
pull request before doing anything else"
            ),
        }
    }

    /// The comment's opening line.
    ///
    /// Varies by hold because one of them must not assert a negative: for
    /// [`MergeHold::MergeResultUnknown`] the module does not know whether
    /// the merge happened, and "PR #7 was not merged" would be a claim it
    /// cannot support.
    fn headline(&self, pr_number: u64) -> String {
        match self {
            MergeHold::MergeResultUnknown { .. } => {
                format!("**Autopilot: PR #{pr_number} may or may not have been merged.**")
            }
            _ => format!("**Autopilot: PR #{pr_number} was not merged.**"),
        }
    }

    /// Whether this hold leaves the pull request open.
    ///
    /// A positive list, not an exclusion list. Written as "everything except
    /// `MergeResultUnknown`" it told a human "**The pull request stays
    /// open**" three lines under "the PR is CLOSED, not open" — a hold whose
    /// entire content is that the PR is *not* open. Enumerating the holds
    /// that genuinely leave it open means a variant added later has to opt
    /// in rather than inherit a claim that may not be true of it.
    fn pr_definitely_still_open(&self) -> bool {
        matches!(
            self,
            MergeHold::Review(_)
                | MergeHold::NotReviewed
                | MergeHold::ReviewHeadUnknown { .. }
                | MergeHold::ReviewIsStale { .. }
                | MergeHold::PrIsDraft
                | MergeHold::PrNotMergeable { .. }
                | MergeHold::BaseBranchMismatch { .. }
                | MergeHold::HumanApprovalRequired { .. }
                | MergeHold::ProtectionUnknown { .. }
                | MergeHold::MergeCommandFailed { .. }
        )
    }
}

/// First 8 characters of a SHA, or the whole string if it is shorter.
pub(crate) fn short_sha(sha: &str) -> String {
    sha.chars().take(8).collect()
}

// ── outcome ─────────────────────────────────────────────────────────────

/// What [`execute_merge`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum MergeOutcome {
    /// The PR was merged.
    Merged {
        strategy: MergeStrategy,
        head_sha: String,
    },
    /// Every guard passed but `dry_run` was set, so nothing was written to
    /// GitHub. Reported as its own variant rather than as a `Merged` with a
    /// flag, so no consumer can mistake a rehearsal for a merge.
    WouldMerge {
        strategy: MergeStrategy,
        head_sha: String,
    },
    /// The PR was already merged before Autopilot got here — by a human, or
    /// by an earlier run whose `gh pr merge` landed the merge and then failed
    /// on a later step.
    ///
    /// Not a [`MergeHold`], and that is the whole reason it exists. A hold
    /// means "this PR is still open and waiting on somebody"; routing an
    /// already-merged PR through one made Autopilot post *"PR #7 was not
    /// merged … the pull request stays open and is labeled `agent:blocked`"*
    /// on a PR that was merged and closed, and park the issue in the one
    /// label that auto-resumes — re-queueing work that had already landed.
    AlreadyMerged { head_sha: String },
    /// The PR was not merged, and why.
    Held(MergeHold),
}

impl MergeOutcome {
    /// Whether *this run* performed the merge. Deliberately excludes
    /// [`MergeOutcome::AlreadyMerged`]: the audit trail's question is what
    /// Autopilot did, and "found it merged" is not "merged it".
    pub fn merged(&self) -> bool {
        matches!(self, MergeOutcome::Merged { .. })
    }

    /// Whether the PR is merged now, however it got that way. The question
    /// the *label* transition asks, as against the one the record asks.
    pub fn landed(&self) -> bool {
        matches!(
            self,
            MergeOutcome::Merged { .. } | MergeOutcome::AlreadyMerged { .. }
        )
    }

    /// The hold, if this outcome is one.
    pub fn hold(&self) -> Option<&MergeHold> {
        match self {
            MergeOutcome::Held(h) => Some(h),
            _ => None,
        }
    }
}

/// The full result of one merge attempt.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct MergeExecution {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub pr_number: u64,
    /// Flattened, for the reason [`super::stagnation::ExhaustExecution`]'s
    /// twin field is: [`MergeOutcome`] is already internally tagged on
    /// `"outcome"`, so nesting it under a field of the same name emitted
    /// `{"outcome":{"outcome":"merged",…}}` — a doubly-nested key nothing
    /// asked for.
    #[serde(flatten)]
    pub outcome: MergeOutcome,
    /// The PR as GitHub described it, when we got far enough to ask.
    pub snapshot: Option<PrSnapshot>,
    /// The label transition applied to the issue. `None` when no `agent:*`
    /// label was touched — a dry run, or a hold on an already-exhausted
    /// issue.
    pub label_plan: Option<labels::LabelPlan>,
    /// Why the label transition did not happen, when the outcome was a
    /// merge and the label write failed.
    ///
    /// A merge cannot be undone, so its report must survive a later failure
    /// rather than being replaced by one. `None` on every other outcome,
    /// where a label failure is still an `Err`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label_error: Option<String>,
    /// Whether a notification comment was posted.
    pub commented: bool,
    /// Whether a *new* merge record was appended, as against this attempt
    /// matching the one already at the head of this PR's history. A poll
    /// loop on an unchanging hold appends nothing after its first pass.
    pub record_appended: bool,
    /// The drawer id of the merge record describing this attempt — the one
    /// just appended, or the identical existing one when
    /// `record_appended` is false. Always present: a storage failure is an
    /// `Err` rather than a missing id.
    pub record_drawer_id: String,
}

pub(crate) fn serialize_issue<S: serde::Serializer>(
    issue: &IssueRef,
    s: S,
) -> Result<S::Ok, S::Error> {
    s.serialize_str(&issue.canonical())
}

/// Inputs to [`execute_merge`].
pub struct MergeRequest<'a> {
    pub issue: &'a IssueRef,
    pub pr_number: u64,
    /// Whether the repo's approved gate is green **right now**. Supplied by
    /// the caller for exactly the reason [`super::review::decide_merge`]
    /// takes it as a parameter: this module never runs a gate, and making the
    /// caller state it keeps "we merged without checking" inexpressible.
    pub gate_green: bool,
    pub strategy: MergeStrategy,
    /// Whether to delete the head branch after a successful merge.
    pub delete_branch: bool,
    /// Perform every check and every read, write nothing. The rehearsal an
    /// irreversible action deserves.
    pub dry_run: bool,
}

// ── execution ───────────────────────────────────────────────────────────

/// Execute rung 5's merge decision for one PR.
///
/// Never returns `Ok` with a merge that skipped a guard: every early return
/// below is a hold, and the `gh pr merge` call is reachable only by falling
/// through all of them — the same shape as
/// [`super::review::decide_merge`]'s.
pub fn execute_merge(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    request: &MergeRequest,
) -> Result<MergeExecution, MemoryError> {
    validate_repo(&request.issue.repo)?;
    let (outcome, snapshot) = evaluate(db, gh_runner, request)?;
    finish(db, gh_runner, request, outcome, snapshot)
}

/// Run every guard and report what the merge *is*, performing the merge
/// itself but none of the bookkeeping around it.
///
/// Split from [`finish`] so each guard reads as its own answer —
/// `return Ok((MergeOutcome::Held(MergeHold::PrIsDraft), Some(snapshot)))` —
/// rather than burying the reason three arguments into a five-line call.
/// [`execute_merge`] is then the whole shape in two lines: decide, then
/// record and notify exactly once.
///
/// The `gh pr merge` call is reachable only by falling through every guard
/// below, the same construction [`super::review::decide_merge`] uses.
fn evaluate(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    request: &MergeRequest,
) -> Result<(MergeOutcome, Option<PrSnapshot>), MemoryError> {
    // 1. The PR as GitHub sees it now — before any storage guard, and that
    //    ordering is load-bearing.
    //
    //    This read used to come third, after the review lookup and rung 5's
    //    gate, so that a PR with no recorded review could be refused without
    //    a single API call. That saving bought a wrong answer: an
    //    already-merged PR whose review was missing, or whose gate had since
    //    gone red, or whose reviewer said `needs_changes` before a human
    //    merged it anyway, never reached the merged check at all. It was
    //    told "PR #N was not merged … the pull request stays open" and
    //    parked in `agent:blocked`, which auto-resumes. Three reachable
    //    paths, all of them saying something false about a PR that had
    //    landed.
    //
    //    One `gh pr view` is a cheap price for never asserting the state of
    //    a pull request without having looked at it.
    let snapshot = gh::pr_snapshot(gh_runner, &request.issue.repo, request.pr_number)?;

    if snapshot.state.eq_ignore_ascii_case("merged") {
        let head_sha = snapshot.head_ref_oid.clone();
        return Ok((MergeOutcome::AlreadyMerged { head_sha }, Some(snapshot)));
    }

    // 2. Is it open? Directly after the merged check and *before* the
    //    storage guards, for the same reason the snapshot itself moved to
    //    the front: every hold below this line describes a pull request that
    //    is still open — "the review is stale", "the base moved", "nobody
    //    reviewed it" — and the comment those holds render says so in as
    //    many words. Reached on a closed PR, they told a human "**The pull
    //    request stays open**" about a PR somebody had closed. Whether it is
    //    open is knowable here, from a snapshot already in hand, so no hold
    //    that assumes it should be reachable before it is checked.
    if !snapshot.state.eq_ignore_ascii_case("open") {
        let state = snapshot.state.clone();
        return Ok((
            MergeOutcome::Held(MergeHold::PrNotOpen { state }),
            Some(snapshot),
        ));
    }

    // 3. The review.
    let Some(review) = latest_review_for_pr(db, request.issue, request.pr_number)? else {
        return Ok((MergeOutcome::Held(MergeHold::NotReviewed), Some(snapshot)));
    };

    // 4. Rung 5's gate, re-derived against the *present* gate state.
    //
    //    Built directly, with no fabricated fields: `decide_merge` takes
    //    exactly the three facts it decides on, so this layer supplies
    //    everything the decision needs rather than padding a wider struct
    //    with `None`s it has no way to know.
    let decision = decide_merge(
        request.gate_green,
        &review.dispatch_class,
        ReviewJudgment {
            process_success: review.process_success,
            verdict: review.verdict,
            risk_class: review.risk_class,
        },
    );
    if let MergeDecision::HoldForHuman(reason) = decision {
        return Ok((
            MergeOutcome::Held(MergeHold::Review(reason)),
            Some(snapshot),
        ));
    }

    if snapshot.is_draft {
        return Ok((MergeOutcome::Held(MergeHold::PrIsDraft), Some(snapshot)));
    }

    // 5. Did the reviewer read *this* commit?
    let Some(reviewed_head) = review.head_sha.clone() else {
        return Ok((
            MergeOutcome::Held(MergeHold::ReviewHeadUnknown {
                reviewed_at: review.recorded_at.clone(),
            }),
            Some(snapshot),
        ));
    };
    if !reviewed_head.eq_ignore_ascii_case(&snapshot.head_ref_oid) {
        let current_head = snapshot.head_ref_oid.clone();
        return Ok((
            MergeOutcome::Held(MergeHold::ReviewIsStale {
                reviewed_head,
                current_head,
            }),
            Some(snapshot),
        ));
    }

    // 6. Is it still the same merge? A retargeted PR was reviewed against a
    //    diff that no longer exists. `None` means the review predates the
    //    field — unknown, so the comparison is skipped rather than reported
    //    as a mismatch; the head-SHA guard above still protects the diff.
    let reviewed_base = review.base_branch.as_deref().unwrap_or_default();
    if !reviewed_base.is_empty() && reviewed_base != snapshot.base_ref_name {
        let hold = MergeHold::BaseBranchMismatch {
            reviewed_base: reviewed_base.to_string(),
            current_base: snapshot.base_ref_name.clone(),
        };
        return Ok((MergeOutcome::Held(hold), Some(snapshot)));
    }

    // 7. Branch protection — the ladder's open question.
    //
    //    **Before** the mergeability guard, and that ordering is the whole
    //    point. GitHub reports `mergeStateStatus: BLOCKED` for a PR that is
    //    merely missing its required approval, so checking mergeability first
    //    made this guard unreachable in exactly the configuration it exists
    //    for: the human got "GitHub reports mergeStateStatus=BLOCKED", which
    //    names a symptom, instead of "main requires 1 approving review(s)",
    //    which names the thing they can do something about.
    //
    //    A rule that *is* satisfied is not a hold. `branch_protection`
    //    describes the branch, not this PR — reading only that refused an
    //    approved PR forever, because the rule still says an approval is
    //    required after one has been given. `reviewDecision` is GitHub's own
    //    answer for this PR, and it accounts for CODEOWNERS as well as the
    //    count, so it is the right question to ask of both requirements.
    let hold = match gh::branch_protection(gh_runner, &request.issue.repo, &snapshot.base_ref_name)?
    {
        BranchProtection::NoHumanApprovalRequired => None,
        BranchProtection::HumanApprovalRequired { .. } if snapshot.human_approved() => None,
        BranchProtection::HumanApprovalRequired {
            required_approving_review_count,
            require_code_owner_reviews,
            enforce_admins,
        } => Some(MergeHold::HumanApprovalRequired {
            base: snapshot.base_ref_name.clone(),
            required_approving_review_count,
            require_code_owner_reviews,
            enforce_admins,
            review_decision: snapshot.review_decision.clone(),
        }),
        BranchProtection::Unknown { detail } => Some(MergeHold::ProtectionUnknown { detail }),
    };
    if let Some(hold) = hold {
        return Ok((MergeOutcome::Held(hold), Some(snapshot)));
    }

    // 8. Would GitHub take the merge at all? Last of the read-only guards,
    //    because it is the least specific: everything above names a cause,
    //    this names only the symptom GitHub reports.
    if !snapshot.mergeable.eq_ignore_ascii_case("mergeable")
        || !mergeable_state_permits(&snapshot.merge_state_status)
    {
        let hold = MergeHold::PrNotMergeable {
            mergeable: snapshot.mergeable.clone(),
            merge_state_status: snapshot.merge_state_status.clone(),
        };
        return Ok((MergeOutcome::Held(hold), Some(snapshot)));
    }

    // 9. Every guard passed.
    let head_sha = snapshot.head_ref_oid.clone();
    if request.dry_run {
        return Ok((
            MergeOutcome::WouldMerge {
                strategy: request.strategy,
                head_sha,
            },
            Some(snapshot),
        ));
    }

    let argv = gh::pr_merge_argv_at(
        &request.issue.repo,
        request.pr_number,
        request.strategy,
        request.delete_branch,
        &head_sha,
    );
    let out = gh_runner.run(&argv)?;
    if !out.success {
        // A non-zero exit is not proof that no merge happened. `gh pr merge`
        // performs the merge and *then* deletes the head branch, so a
        // deletion GitHub refuses — a ruleset that forbids it, a 403 —
        // exits non-zero on a PR that is merged; so does losing the response
        // to a timeout after the mutation applied. Reporting a hold there
        // told a human "PR #N was not merged" about a merged PR and parked
        // the issue in `agent:blocked`, which auto-resumes and re-queues
        // work that has already landed.
        //
        // So: ask. A re-read that says `MERGED` is the authoritative answer
        // and outranks the exit code. A re-read that fails answers nothing,
        // and is discarded rather than propagated — the merge command's own
        // failure is the more useful thing to report.
        let detail = format!(
            "exit {:?}: {}",
            out.code,
            scrub_and_bound(out.stderr.trim(), MAX_LINEAGE_FIELD_CHARS).text
        );
        let after = gh::pr_snapshot(gh_runner, &request.issue.repo, request.pr_number);
        let hold = match after {
            // Merged — but by whom? `--match-head-commit` makes GitHub
            // refuse if the head moved, so a 409 *plus* a merged PR whose
            // head is not the commit we named means somebody else merged a
            // newer commit while we were asking. Reporting that as
            // `Merged` would write "Autopilot merged this" into the audit
            // trail for an action Autopilot did not take, and would pair a
            // `head_sha` we tried to merge with a record naming the one
            // that actually landed.
            Ok(ref s) if s.state.eq_ignore_ascii_case("merged") => {
                let landed = s.head_ref_oid.clone();
                let outcome = if landed.eq_ignore_ascii_case(&head_sha) {
                    MergeOutcome::Merged {
                        strategy: request.strategy,
                        head_sha,
                    }
                } else {
                    MergeOutcome::AlreadyMerged { head_sha: landed }
                };
                return Ok((outcome, after.ok()));
            }
            // The re-read answered and said the PR is still open. The exit
            // code is then the whole story and may be reported as one.
            Ok(ref s) if s.state.eq_ignore_ascii_case("open") => {
                MergeHold::MergeCommandFailed { detail }
            }
            // It answered something else — `CLOSED`, most likely a human
            // closing the PR while the merge was being attempted. Reporting
            // `MergeCommandFailed` there produced a comment claiming "the
            // pull request stays open" about a closed PR, and a record
            // asserting a state nothing had confirmed.
            Ok(ref s) => MergeHold::PrNotOpen {
                state: s.state.clone(),
            },
            // The re-read did not answer. Discarding this error and
            // reporting `MergeCommandFailed` would turn "we could not ask"
            // into "the merge did not happen" — the one inversion this
            // module exists to prevent. The error is carried into the hold
            // so the human sees *both* failures, not just the first.
            Err(ref e) => MergeHold::MergeResultUnknown {
                detail,
                read_failure: scrub_and_bound(&e.to_string(), MAX_LINEAGE_FIELD_CHARS).text,
            },
        };
        return Ok((MergeOutcome::Held(hold), after.ok().or(Some(snapshot))));
    }

    Ok((
        MergeOutcome::Merged {
            strategy: request.strategy,
            head_sha,
        },
        Some(snapshot),
    ))
}

/// Whether GitHub's `mergeStateStatus` permits a merge.
///
/// # A known limitation, stated rather than hidden
///
/// `UNKNOWN` — and [`MergeHold::ProtectionUnknown`] with it — is often
/// *transient*: GitHub has not finished computing the answer and would give
/// a real one seconds later. Rung 6 nonetheless escalates it like any other
/// hold, with a comment and `agent:blocked`, because it has no retry or
/// backoff of its own and the alternative is worse: silently declining to
/// notify anybody would leave a genuinely stuck PR spinning with no human
/// ever told. Escalating a self-resolving condition costs one comment, which
/// the dedup bounds to exactly one, and a label a human can clear. That is
/// the right side to err on until a rung adds bounded retries.
///
/// Only `CLEAN` and `UNSTABLE` do. `UNSTABLE` means non-required checks are
/// failing or pending while every *required* one has passed — GitHub will
/// merge it, and refusing would make Autopilot unable to merge in any repo
/// with an optional check. Everything else, `UNKNOWN` included, holds: an
/// answer GitHub has not finished computing is not a yes.
fn mergeable_state_permits(status: &str) -> bool {
    status.eq_ignore_ascii_case("clean") || status.eq_ignore_ascii_case("unstable")
}

/// Apply the outcome's side effects — label, comment, lineage record — and
/// assemble the report.
///
/// Every terminal path in [`execute_merge`] funnels through here so that no
/// outcome can be returned without the label and notification the spec
/// attaches to it ("PR stays open, **labeled, human notified**"), and so that
/// the record is written exactly once.
///
/// # Where the record goes in the order, and why it differs by outcome
///
/// A merge records **first**: it has already happened and cannot be undone,
/// so no later failure may be allowed to take its audit entry with it.
///
/// A hold records **last**: nothing irreversible has happened, and the
/// record is itself what suppresses the next run's comment. Recording first
/// meant a comment that failed to post still left a record saying the hold
/// had been reported — and the human was then never told, on that run or any
/// later one, because the reason had not changed.
///
/// The two orderings are not a inconsistency to be tidied away. They are the
/// same rule applied to different stakes: never lose the record of something
/// that cannot be undone, and never claim to have said something that was
/// not said.
fn finish(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    request: &MergeRequest,
    outcome: MergeOutcome,
    snapshot: Option<PrSnapshot>,
) -> Result<MergeExecution, MemoryError> {
    // Read before anything is appended, or it would always find itself.
    let previous = last_record_for_pr(db, request.issue, request.pr_number, request.dry_run)?;

    let record = MergeRecord {
        issue: request.issue.clone(),
        pr_number: request.pr_number,
        gate_green: request.gate_green,
        dry_run: request.dry_run,
        outcome: outcome.clone(),
        base_branch: snapshot.as_ref().map(|s| s.base_ref_name.clone()),
        head_sha: snapshot.as_ref().map(|s| s.head_ref_oid.clone()),
    };
    let repeat = previous.filter(|p| p.says_the_same_as(&record));

    let mut commented = false;
    let mut label_plan = None;
    let mut label_error = None;
    let record_drawer_id;

    match (&outcome, request.dry_run) {
        // A rehearsal writes nothing to GitHub; the record is the only trace
        // it leaves, and a repeated rehearsal leaves none.
        (_, true) | (MergeOutcome::WouldMerge { .. }, _) => {
            record_drawer_id = record_once(db, &record, repeat.as_ref())?;
        }
        // Recorded *before* the label write: by this point the merge has
        // happened and cannot be undone, so a `gh issue edit` that failed —
        // a 403, a rate limit, a dropped connection — must not propagate out
        // of `finish` and take the record of the one irreversible action
        // Autopilot performs with it. Recording first costs a
        // never-executed record only when *storage* fails, which is before
        // anything has been written to GitHub at all.
        (MergeOutcome::Merged { .. }, _) => {
            record_drawer_id = record_once(db, &record, repeat.as_ref())?;
            // Clear every `agent:*` label. A merged issue that keeps
            // `agent:ready` is re-picked by the next poll forever — the
            // same budget livelock the spec's stagnation control exists
            // to prevent, arrived at from the opposite direction.
            //
            // A failure here is reported, not propagated. `?` here threw
            // away a fully-populated `MergeExecution` carrying
            // `MergeOutcome::Merged` and the record id, so a merge that
            // *succeeded* and then hit a 403 on `gh issue edit` was
            // indistinguishable, to every caller, from one that never
            // happened. The merge is the irreversible part and the caller
            // has to be told about it; the label is retried on the next
            // pass, which now recognises the PR as `AlreadyMerged` and
            // clears the labels then.
            (label_plan, label_error) =
                split_label_result(labels::set_exclusive_label(gh_runner, request.issue, None));
        }
        // Same terminal state as a merge Autopilot performed itself: the
        // work has landed, so no `agent:*` label should keep the issue in
        // any queue. Commented once — `repeat` suppresses the second — so
        // the issue records that Autopilot saw it and stood down.
        (MergeOutcome::AlreadyMerged { .. }, _) => {
            // The labels are cleared *before* the comment is written, for
            // the reason the hold arm reads them first: the comment says
            // what happened to them. Commenting first asserted "every
            // `agent:*` label has been cleared from this issue" and only
            // then attempted the clear — so a 403 left that sentence
            // standing and false, with the dedup below guaranteeing no
            // later run would ever correct it.
            //
            // A label write that fails must not erase the fact that the PR
            // has landed, so it is reported rather than propagated — the
            // same rule as the `Merged` arm.
            (label_plan, label_error) =
                split_label_result(labels::set_exclusive_label(gh_runner, request.issue, None));
            if repeat.is_none() {
                gh::comment_on_issue(
                    gh_runner,
                    request.issue,
                    &render_already_merged_comment(request, label_error.is_none()),
                )?;
                commented = true;
            }
            record_drawer_id = record_once(db, &record, repeat.as_ref())?;
        }
        // Recorded *after* the comment, and the opposite of the merge case
        // for the opposite reason: nothing irreversible has happened, and
        // the record is what suppresses the next run's comment. Recording
        // first meant a comment that failed to post left behind a record
        // claiming the hold had been reported, and the human was then never
        // told about that hold — on any later run — because the reason had
        // not changed.
        (MergeOutcome::Held(hold), _) => {
            // The labels are read *first*, before the comment is written,
            // because the comment names the label the issue will carry —
            // and on an exhausted issue that is not `agent:blocked`. Writing
            // a fixed string first told the human "labeled `agent:blocked`"
            // on every hold against an exhausted issue, which was both false
            // and, thanks to the dedup below, never corrected.
            let current = gh::issue_labels(gh_runner, request.issue)?;
            let stop = hold_stop_label(&current);

            // Not re-posted when the previous attempt on this PR held for
            // the *same* reason: `HumanApprovalRequired` on a protected
            // branch never resolves on its own, so a poll loop would
            // otherwise bury the issue in identical comments — the shape
            // `exhaust_issue` already refuses. A hold that has *changed*
            // is new information and is still commented on.
            if repeat.is_none() {
                gh::comment_on_issue(
                    gh_runner,
                    request.issue,
                    &render_hold_comment(request, hold, stop),
                )?;
                commented = true;
            }
            record_drawer_id = record_once(db, &record, repeat.as_ref())?;
            label_plan = match stop {
                // Already carrying the permanent stop sign: nothing to do,
                // and `plan_exclusive` toward `agent:blocked` would take it
                // down. See [`hold_stop_label`].
                AgentLabel::Exhausted => None,
                target => Some(labels::apply_plan(
                    gh_runner,
                    request.issue,
                    labels::plan_exclusive(&current, Some(target)),
                )?),
            };
        }
    }

    Ok(MergeExecution {
        issue: request.issue.clone(),
        pr_number: request.pr_number,
        outcome,
        snapshot,
        label_plan,
        label_error,
        commented,
        record_appended: repeat.is_none(),
        record_drawer_id,
    })
}

/// Split a label write's result into "what was applied" and "what went
/// wrong", so a caller can report both rather than choosing one.
///
/// Used only on the two outcomes where the pull request has already landed.
/// Everywhere else a label failure is still an `Err`: nothing irreversible
/// has happened, so stopping and retrying loses nothing.
fn split_label_result(
    result: Result<labels::LabelPlan, MemoryError>,
) -> (Option<labels::LabelPlan>, Option<String>) {
    match result {
        Ok(plan) => (Some(plan), None),
        Err(e) => (None, Some(e.to_string())),
    }
}

/// Append `record`, unless an identical one is already at the head of this
/// PR's history — in which case return *its* drawer id.
///
/// Merge records share the issue entity with attempts and reviews, and a
/// hold that never resolves is polled indefinitely; appending an identical
/// record on every pass would grow the trail without adding a fact. The
/// audit question a reader asks is "what did Autopilot do, and when did it
/// change its mind" — repeats answer neither.
fn record_once(
    db: &Database,
    record: &MergeRecord,
    repeat: Option<&RecordedMergeSummary>,
) -> Result<String, MemoryError> {
    match repeat {
        Some(previous) => Ok(previous.drawer_id.clone()),
        None => record_merge(db, record),
    }
}

/// Which `agent:*` label an issue should carry after a hold.
///
/// `agent:blocked` is the right label for a held PR — "awaiting a human" is
/// exactly what it means, and its auto-resume-on-a-newer-human-comment
/// semantics are the right ones, since a human who approves or comments has
/// supplied the thing that was missing. The spec defines three labels and
/// inventing a fourth here would put a state in the repo that nothing else
/// in the design knows how to clear.
///
/// But `set_exclusive_label` removes every *other* `agent:*` label, so
/// applying it unconditionally took an exhausted issue — one the spec says
/// **never self-resumes** — and moved it to the one label that does. An
/// exhausted issue therefore keeps `agent:exhausted`: the stop sign stays
/// up, and the comment still explains why this attempt stopped.
///
/// Pure, so the comment and the label write cannot disagree about the
/// answer: both are derived from one call on one reading of the labels.
fn hold_stop_label(current: &[String]) -> AgentLabel {
    if current
        .iter()
        .any(|l| AgentLabel::from_label_str(l) == Some(AgentLabel::Exhausted))
    {
        AgentLabel::Exhausted
    } else {
        AgentLabel::Blocked
    }
}

/// The comment posted when Autopilot finds the PR already merged.
///
/// Its own text rather than a [`render_hold_comment`] with a different
/// reason: every sentence in that one — "was not merged", "stays open",
/// "labeled for a human", "re-labeling puts it back in the queue" — is false
/// of a PR that has landed.
/// `labels_cleared` is passed in rather than assumed: this comment used to
/// state the clearing as accomplished fact while being written *before* the
/// attempt, so a refused `gh issue edit` left a false sentence on the issue
/// that no later run would correct.
pub fn render_already_merged_comment(request: &MergeRequest, labels_cleared: bool) -> String {
    let labels = if labels_cleared {
        format!(
            "Every `agent:*` label has been cleared from this issue so it is not picked up \
again; re-label it `{}` if there is more to do.",
            AgentLabel::Ready.as_str()
        )
    } else {
        format!(
            "The `agent:*` labels could **not** be cleared from this issue, so it may be \
picked up again — remove them by hand, or leave `{}` on it if there is more to do.",
            AgentLabel::Ready.as_str()
        )
    };
    let body = format!(
        "**Autopilot: PR #{pr} is already merged.**\n\n\
Autopilot did not merge it on this run. {labels}\n\n\
<sub>Autopilot rung 6.</sub>",
        pr = request.pr_number,
    );
    scrub_and_bound(&body, MAX_COMMENT_CHARS).text
}

/// The comment posted on a held PR's issue.
///
/// Pure, so the exact text a human sees is asserted in tests rather than
/// discovered in production. Scrubbed and bounded because a hold reason can
/// quote `gh`'s stderr and a review's reason, both of which quote the diff.
///
/// `stop` is the label the issue will actually carry, passed in rather than
/// assumed: the sentence naming it was once a fixed string, and on an
/// exhausted issue it named a label that was deliberately not applied.
pub fn render_hold_comment(request: &MergeRequest, hold: &MergeHold, stop: AgentLabel) -> String {
    let disposition = match stop {
        AgentLabel::Exhausted => format!(
            "The issue keeps `{}` and Autopilot will not retry it; a human must re-label it \
`{}` to put it back in the queue.",
            AgentLabel::Exhausted.as_str(),
            AgentLabel::Ready.as_str()
        ),
        target => format!(
            "{}It is labeled `{}` for a human. Autopilot will not merge it on its own; \
re-labeling the issue `{}` after resolving the above puts it back in the queue.",
            if hold.pr_definitely_still_open() {
                "The pull request stays open. "
            } else {
                ""
            },
            target.as_str(),
            AgentLabel::Ready.as_str()
        ),
    };
    let body = format!(
        "{headline}\n\n{summary}.\n\n{disposition}\n\n\
<sub>Gate reported {gate} at merge time. Autopilot rung 6.</sub>",
        headline = hold.headline(request.pr_number),
        summary = hold.summary(),
        gate = if request.gate_green {
            "green"
        } else {
            "not green"
        },
    );
    scrub_and_bound(&body, MAX_COMMENT_CHARS).text
}

// ── storage ─────────────────────────────────────────────────────────────

const MERGE_ENTITY_TYPE: &str = "merge";
const HAS_MERGE_PREDICATE: &str = "has_merge";

/// One merge attempt, as the caller supplies it.
#[derive(Debug, Clone, PartialEq)]
pub struct MergeRecord {
    pub issue: IssueRef,
    pub pr_number: u64,
    pub gate_green: bool,
    pub dry_run: bool,
    pub outcome: MergeOutcome,
    pub base_branch: Option<String>,
    pub head_sha: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MergeBody {
    issue: String,
    repo: String,
    issue_number: u64,
    pr_number: u64,
    gate_green: bool,
    dry_run: bool,
    outcome: MergeOutcome,
    base_branch: Option<String>,
    head_sha: Option<String>,
    /// Same role as `lineage::AttemptRecord`'s and `review::ReviewBody`'s:
    /// guarantees this record's content — and so its content-derived drawer
    /// id — is unique even when two merge attempts agree in every field, so
    /// the second cannot silently overwrite the first. Merge attempts repeat:
    /// a held PR is re-attempted after a human resolves the hold.
    record_id: String,
    recorded_at: String,
}

/// Append a merge record to the issue's lineage.
///
/// **Append-only, no `logical_key`** — the seventh drawer kind, of the same
/// shape as attempts and reviews and for the same reason: a hold and a later
/// merge of the same PR are two facts, and keying them would destroy the
/// first. This is the audit trail for the only irreversible thing Autopilot
/// does.
///
/// # Append-only is about *transitions*, not about invocations
///
/// [`record_once`] declines to call this when the attempt is identical to
/// the one already at the head of the PR's history, and that is not a
/// weakening of the guarantee above. Every state this PR has been in is
/// still recorded, in order, and none overwrites another. What is not
/// recorded is the tenth consecutive poll finding the same unresolved hold —
/// which adds no fact, and which this module already declined to *comment*
/// on for exactly that reason. Extending the same judgment from the comment
/// to the record makes the two consistent; treating a repeated invocation as
/// a new fact was what let a poll loop grow the trail without end.
pub fn record_merge(db: &Database, record: &MergeRecord) -> Result<String, MemoryError> {
    validate_repo(&record.issue.repo)?;

    let body = MergeBody {
        issue: record.issue.canonical(),
        repo: record.issue.repo.clone(),
        issue_number: record.issue.number,
        pr_number: record.pr_number,
        gate_green: record.gate_green,
        dry_run: record.dry_run,
        outcome: record.outcome.clone(),
        base_branch: record.base_branch.clone(),
        head_sha: record.head_sha.clone(),
        record_id: uuid::Uuid::new_v4().to_string(),
        recorded_at: chrono::Utc::now().to_rfc3339(),
    };

    let content = serde_json::to_string(&body)?;
    let drawer_id = crate::db::drawers::generate_id(&content, WING, ROOM);
    let issue_entity = record.issue.entity_name();
    let embedding = zero_embedding();

    db.with_transaction(|tx| {
        Database::insert_drawer_tx(
            tx, &drawer_id, &content, &embedding, WING, ROOM, "", ADDED_BY,
        )?;
        KnowledgeGraph::add_triple_tx(
            tx,
            &issue_entity,
            ISSUE_ENTITY_TYPE,
            HAS_MERGE_PREDICATE,
            &drawer_id,
            MERGE_ENTITY_TYPE,
            None,
            1.0,
            None,
        )?;
        Database::wal_log_tx(
            tx,
            "autopilot_record_merge",
            &json!({
                "drawer_id": &drawer_id,
                "issue": &body.issue,
                "pr_number": body.pr_number,
                "merged": body.outcome.merged(),
            }),
            None,
        )?;
        Ok(())
    })?;

    Ok(drawer_id)
}

/// One recorded merge attempt, read back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RecordedMergeSummary {
    /// The drawer this was read out of, so a caller that decides *not* to
    /// append a duplicate can still name the record that says the same
    /// thing.
    pub drawer_id: String,
    /// The issue this record hangs off, in canonical `repo#number` form.
    /// Read back rather than assumed — see [`Self::says_the_same_as`].
    pub issue: String,
    pub pr_number: u64,
    pub gate_green: bool,
    pub dry_run: bool,
    pub outcome: MergeOutcome,
    pub base_branch: Option<String>,
    pub head_sha: Option<String>,
    pub recorded_at: String,
}

impl RecordedMergeSummary {
    /// Whether this record already states everything `candidate` would.
    ///
    /// Deliberately every field that describes the *attempt* and none that
    /// describe the *writing* of it: `recorded_at` and the record's uuid
    /// differ on every pass by construction, so including them would make
    /// nothing ever a repeat.
    fn says_the_same_as(&self, candidate: &MergeRecord) -> bool {
        // `issue` is compared even though `last_record_for_pr` reads only
        // this issue's own lineage. That lineage is keyed by
        // `IssueRef::entity_name()`, whose `repo_slug` maps every character
        // outside `[A-Za-z0-9_.-]` to `-` — so `owner/repo#42` and
        // `owner-repo#42` share one entity and one history. Without this
        // line a hold on the second would be suppressed as a repeat of the
        // first: no comment, no record, and a human never told. Asserting an
        // invariant is cheaper than trusting it.
        self.issue
            .eq_ignore_ascii_case(&candidate.issue.canonical())
            && self.pr_number == candidate.pr_number
            && self.gate_green == candidate.gate_green
            && self.dry_run == candidate.dry_run
            && self.outcome == candidate.outcome
            && self.base_branch == candidate.base_branch
            && self.head_sha == candidate.head_sha
    }
}

/// Every recorded merge attempt for an issue, oldest first.
pub fn merges_for_issue(
    db: &Database,
    issue: &IssueRef,
) -> Result<Vec<RecordedMergeSummary>, MemoryError> {
    let kg = KnowledgeGraph::new(db);
    let entity = match kg.resolve_entity(&issue.entity_name(), Some(ISSUE_ENTITY_TYPE)) {
        Ok(entity) => entity,
        Err(MemoryError::NotFound(_)) => return Ok(Vec::new()),
        Err(other) => return Err(other),
    };

    let triples =
        kg.query_entity_current_with_predicate(&entity.id, HAS_MERGE_PREDICATE, MAX_ISSUE_EDGES)?;
    let mut records = Vec::new();
    for triple in triples {
        let Some(object_entity) = kg.get_entity(&triple.object)? else {
            continue;
        };
        let Some(drawer) = db.get_drawer(&object_entity.name)? else {
            continue;
        };
        let body: MergeBody = serde_json::from_str(&drawer.content)?;
        records.push(RecordedMergeSummary {
            drawer_id: object_entity.name.clone(),
            issue: body.issue,
            pr_number: body.pr_number,
            gate_green: body.gate_green,
            dry_run: body.dry_run,
            outcome: body.outcome,
            base_branch: body.base_branch,
            head_sha: body.head_sha,
            recorded_at: body.recorded_at,
        });
    }
    // The secondary key keeps the order total. `recorded_at` is an RFC 3339
    // string and sorts correctly, but two records written inside the same
    // nanosecond would tie — and a *stable* sort over a vec the SQL handed
    // back newest-first would then leave the oldest of the tie group last,
    // so `rfind` would take the oldest as "most recent". The drawer id is
    // content-derived and unique, which is enough to make the answer
    // deterministic rather than dependent on the query's arrival order.
    records.sort_by(|a, b| {
        a.recorded_at
            .cmp(&b.recorded_at)
            .then_with(|| a.drawer_id.cmp(&b.drawer_id))
    });
    Ok(records)
}

/// The most recent record for this PR at the same rehearsal-or-real footing
/// as the attempt about to be recorded.
///
/// Matching on `dry_run` is what keeps the two kinds of history from
/// interfering. A rehearsal notifies nobody, so it must never suppress a
/// real comment; and a real attempt must not make the next rehearsal look
/// like a repeat of something that was actually written to GitHub. Comparing
/// only within a footing gives each its own answer to "have we already said
/// this?".
fn last_record_for_pr(
    db: &Database,
    issue: &IssueRef,
    pr_number: u64,
    dry_run: bool,
) -> Result<Option<RecordedMergeSummary>, MemoryError> {
    let records = merges_for_issue(db, issue)?;
    // Filtered on the issue for the reason `says_the_same_as` compares it:
    // `repo_slug` collapses `owner/repo` and `owner-repo` onto one lineage
    // entity. Without this, two colliding issues polling the same PR number
    // never match each other's records — `repeat` is always `None` — and the
    // dedup inverts from losing a notification into an unbounded stream of
    // comments and records.
    // Case-insensitive for the reason `latest_review_for_pr` documents: the
    // entity these records hang off is keyed on a lowercased name, so an
    // exact match would miss this issue's own records whenever the caller
    // spelled the repo differently.
    let canonical = issue.canonical();
    Ok(records.into_iter().rfind(|r| {
        r.issue.eq_ignore_ascii_case(&canonical) && r.pr_number == pr_number && r.dry_run == dry_run
    }))
}

/// The most recent review recorded for this issue **and this PR**.
///
/// Filtering by PR number is not a nicety: an issue's reviews accumulate
/// across PRs, and a `PASS` on an abandoned PR must never authorize merging a
/// different one. Taking the *latest* rather than the latest `pass` is the
/// other half — a `needs_changes` recorded after a `pass` is the current
/// answer, and picking the pass out of the history would let a re-review's
/// finding be ignored by looking further back.
fn latest_review_for_pr(
    db: &Database,
    issue: &IssueRef,
    pr_number: u64,
) -> Result<Option<RecordedReviewSummary>, MemoryError> {
    let reviews: Vec<RecordedReviewSummary> = super::review::reviews_for_issue(db, issue)?;
    // Filtered on the issue as well as the PR, the third place in this
    // module that must not trust `repo_slug`. It collapses `owner/repo` and
    // `owner-repo` onto one lineage entity, and a fork pair — plausibly
    // slug-colliding, plausibly sharing commits — could therefore offer one
    // repo's `PASS` as a candidate review for the other's PR of the same
    // number. The head-SHA guard blocks that only while the two heads
    // differ, which for a fork pair is not something to rely on.
    //
    // Compared case-*insensitively*, and that is not a nicety. `canonical()`
    // preserves whatever case the caller spelled the repo in, while the
    // knowledge-graph entity these records hang off is keyed on a lowercased
    // name — so `--repo ironrace/ironmem` and `--repo ironrace/IronMem`
    // resolve to the same entity and return each other's records, but their
    // canonical strings differ. An exact comparison would then reject a
    // review that genuinely belongs to this issue and report `NotReviewed`,
    // and the PR would never merge. GitHub repo names are ASCII, so
    // `eq_ignore_ascii_case` is the same equivalence the entity id uses; the
    // slug collision this filter exists for differs by `/` versus `-`, not
    // by case, so it is still caught.
    let canonical = issue.canonical();
    Ok(reviews
        .into_iter()
        .rfind(|r| r.issue.eq_ignore_ascii_case(&canonical) && r.pr_number == pr_number))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::GhOutput;
    use crate::autopilot::review::{
        record_review, HoldReason, ReviewOutcome, ReviewRecord, ReviewVerdict, RiskClass,
    };

    fn issue() -> IssueRef {
        IssueRef::new("owner/repo", 42)
    }

    fn db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    const HEAD: &str = "1111111111111111111111111111111111111111";
    const OTHER_HEAD: &str = "2222222222222222222222222222222222222222";

    fn pass_outcome() -> ReviewOutcome {
        ReviewOutcome {
            verdict: Some(ReviewVerdict::Pass),
            risk_class: Some(RiskClass::Documentation),
            reason: Some("docs only".into()),
            total_cost_usd: None,
            token_usage: None,
            process_success: true,
        }
    }

    fn store_review(database: &Database, pr: u64, head: Option<&str>, outcome: ReviewOutcome) {
        store_review_against(database, pr, head, Some("main"), outcome);
    }

    fn store_review_against(
        database: &Database,
        pr: u64,
        head: Option<&str>,
        base: Option<&str>,
        outcome: ReviewOutcome,
    ) {
        let decision = decide_merge(true, "documentation", outcome.judgment());
        record_review(
            database,
            &ReviewRecord {
                issue: issue(),
                pr_number: pr,
                dispatch_class: "documentation".into(),
                head_sha: head.map(|h| h.to_string()),
                base_branch: base.map(|b| b.to_string()),
                outcome,
                decision,
            },
        )
        .expect("review should record");
    }

    fn pr_view_ok(state: &str, head: &str) -> GhOutput {
        pr_view_reviewed(state, head, "")
    }

    /// A PR view carrying an explicit `reviewDecision` — `APPROVED`,
    /// `REVIEW_REQUIRED`, or empty for a base that requires nothing.
    fn pr_view_reviewed(state: &str, head: &str, review_decision: &str) -> GhOutput {
        GhOutput::ok(&format!(
            r#"{{"state":"{state}","isDraft":false,"mergeable":"MERGEABLE",
                "mergeStateStatus":"CLEAN","baseRefName":"main",
                "headRefName":"autopilot/owner-repo-42","headRefOid":"{head}",
                "reviewDecision":"{review_decision}",
                "url":"https://github.com/owner/repo/pull/7"}}"#
        ))
    }

    /// The classic protection endpoint's 404. On its own it no longer
    /// answers the question — [`no_rules`] is what completes it.
    fn unprotected() -> GhOutput {
        GhOutput::failed("", "gh: Branch not protected (HTTP 404)")
    }

    /// The rulesets endpoint reporting no rules in force.
    fn no_rules() -> GhOutput {
        GhOutput::ok("[[]]")
    }

    /// `gh label create` for a label the repo does not have yet. Every plan
    /// that *adds* a label now provisions it first.
    fn label_created() -> GhOutput {
        GhOutput::ok("")
    }

    /// Classic protection requiring one approving review, admins included —
    /// this very repository's configuration.
    fn protected_requiring_one_approval() -> GhOutput {
        GhOutput::ok(
            r#"{"required_pull_request_reviews":{"required_approving_review_count":1},
                "enforce_admins":{"enabled":true}}"#,
        )
    }

    fn request(dry_run: bool) -> MergeRequest<'static> {
        static ISSUE: std::sync::OnceLock<IssueRef> = std::sync::OnceLock::new();
        MergeRequest {
            issue: ISSUE.get_or_init(issue),
            pr_number: 7,
            gate_green: true,
            strategy: MergeStrategy::Squash,
            delete_branch: true,
            dry_run,
        }
    }

    // ── the happy path ──────────────────────────────────────────────────

    #[test]
    fn a_clean_low_risk_pass_on_an_unprotected_branch_merges() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok("merged")),
            // clearing the labels: read, then edit
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.merged(), "{:?}", exec.outcome);
        let merge_argv = &gh_runner.seen[3];
        assert!(merge_argv.contains(&"--squash".to_string()));
        assert!(
            merge_argv.contains(&"--match-head-commit".to_string())
                && merge_argv.contains(&HEAD.to_string()),
            "the merge must pin the reviewed commit: {merge_argv:?}"
        );
    }

    #[test]
    fn a_merged_issue_is_stripped_of_every_agent_label() {
        // Otherwise a stale `agent:ready` makes the Lead re-pick finished
        // work on every poll, forever.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        let plan = exec.label_plan.expect("a merge clears labels");
        assert!(plan.add.is_empty(), "a merged issue gets no new label");
        assert_eq!(plan.remove, vec!["agent:ready".to_string()]);
    }

    #[test]
    fn a_dry_run_checks_everything_and_writes_nothing_to_github() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(true)).unwrap();

        assert!(
            matches!(exec.outcome, MergeOutcome::WouldMerge { .. }),
            "{:?}",
            exec.outcome
        );
        assert_eq!(
            gh_runner.seen.len(),
            3,
            "only the reads happened: the PR view and the two protection lookups"
        );
        assert!(!exec.commented);
        assert!(exec.label_plan.is_none());
    }

    // ── review guards ───────────────────────────────────────────────────

    #[test]
    fn an_unreviewed_pr_is_never_merged_but_github_is_still_asked_first() {
        // This used to assert that an unreviewed PR cost *no* API calls,
        // because the review lookup came first. That saving bought a wrong
        // answer: a PR a human had already merged, whose review was never
        // recorded, was told "PR #7 was not merged … the pull request stays
        // open" and parked in `agent:blocked`. One `gh pr view` is the price
        // of never asserting a pull request's state without looking at it.
        let database = db();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(exec.outcome.hold(), Some(&MergeHold::NotReviewed));
        assert!(
            !gh_runner
                .seen
                .iter()
                .any(|a| a.contains(&"merge".to_string())),
            "nothing may be merged"
        );
        assert!(
            !gh_runner
                .seen
                .iter()
                .any(|a| a.iter().any(|s| s.contains("protection"))),
            "and nothing beyond the PR read is asked: {:?}",
            gh_runner.seen
        );
    }

    #[test]
    fn a_merged_pr_is_recognised_even_when_no_review_was_ever_recorded() {
        // The path the storage-first ordering hid: a human opens, reviews
        // and merges the PR before Autopilot's reviewer ever runs.
        let database = db();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("MERGED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.landed(), "{:?}", exec.outcome);
        assert!(exec.outcome.hold().is_none(), "a landed PR is not a hold");
    }

    #[test]
    fn a_merged_pr_is_recognised_even_when_the_gate_has_since_gone_red() {
        // And the second hidden path: the review passed, the PR was merged,
        // and the gate then went red on an unrelated commit. `decide_merge`
        // held before anyone asked GitHub anything.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("MERGED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(
            &database,
            &mut gh_runner,
            &MergeRequest {
                gate_green: false,
                ..request(false)
            },
        )
        .unwrap();

        assert!(exec.outcome.landed(), "{:?}", exec.outcome);
    }

    #[test]
    fn a_review_recorded_under_a_differently_cased_repo_still_authorizes_the_merge() {
        // `canonical()` keeps the caller's spelling; the knowledge-graph
        // entity is keyed on a lowercased name. So these two resolve to one
        // entity and see each other's records, and an exact string filter
        // would report a reviewed PR as `NotReviewed` — blocking the merge
        // forever on nothing but a capital letter.
        let database = db();
        let recorded_as = IssueRef::new("Owner/Repo", 42);
        let merged_as = IssueRef::new("owner/repo", 42);

        let outcome = pass_outcome();
        let decision = decide_merge(true, "documentation", outcome.judgment());
        record_review(
            &database,
            &ReviewRecord {
                issue: recorded_as,
                pr_number: 7,
                dispatch_class: "documentation".into(),
                head_sha: Some(HEAD.into()),
                base_branch: Some("main".into()),
                outcome,
                decision,
            },
        )
        .unwrap();

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
        ]);
        let exec = execute_merge(
            &database,
            &mut gh_runner,
            &MergeRequest {
                issue: &merged_as,
                pr_number: 7,
                gate_green: true,
                strategy: MergeStrategy::Squash,
                delete_branch: true,
                dry_run: false,
            },
        )
        .unwrap();

        assert!(
            exec.outcome.merged(),
            "a capital letter must not withhold a recorded review: {:?}",
            exec.outcome
        );
    }

    #[test]
    fn a_pass_on_a_slug_colliding_issue_does_not_authorize_this_one() {
        // `repo_slug` maps `/` to `-`, so `own/er-repo#7` and `own-er/repo#7`
        // share one lineage entity — a fork pair, plausibly sharing commits.
        // One repo's `PASS` must not authorize the other's PR of the same
        // number, and the head-SHA guard cannot be relied on to catch it.
        let database = db();
        let other = IssueRef::new("own/er-repo", 42);
        let mine = IssueRef::new("own-er/repo", 42);
        assert_eq!(other.slug(), mine.slug(), "the collision under test");

        let outcome = pass_outcome();
        let decision = decide_merge(true, "documentation", outcome.judgment());
        record_review(
            &database,
            &ReviewRecord {
                issue: other.clone(),
                pr_number: 7,
                dispatch_class: "documentation".into(),
                head_sha: Some(HEAD.into()),
                base_branch: Some("main".into()),
                outcome,
                decision,
            },
        )
        .unwrap();

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);
        let exec = execute_merge(
            &database,
            &mut gh_runner,
            &MergeRequest {
                issue: &mine,
                pr_number: 7,
                gate_green: true,
                strategy: MergeStrategy::Squash,
                delete_branch: true,
                dry_run: false,
            },
        )
        .unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::NotReviewed),
            "another issue's review is not this one's: {:?}",
            exec.outcome
        );
    }

    #[test]
    fn a_pass_on_a_different_pr_does_not_authorize_this_one() {
        // The whole reason `latest_review_for_pr` filters by PR number.
        let database = db();
        store_review(&database, 99, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(exec.outcome.hold(), Some(&MergeHold::NotReviewed));
    }

    #[test]
    fn a_later_needs_changes_overrides_an_earlier_pass() {
        // Taking the latest review rather than the latest *passing* one:
        // otherwise a re-review's finding is ignored by looking further back.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        std::thread::sleep(std::time::Duration::from_millis(5));
        store_review(
            &database,
            7,
            Some(HEAD),
            ReviewOutcome {
                verdict: Some(ReviewVerdict::NeedsChanges),
                risk_class: Some(RiskClass::Documentation),
                reason: Some("found something".into()),
                total_cost_usd: None,
                token_usage: None,
                process_success: true,
            },
        );
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::Review(HoldReason::NeedsChanges))
        );
    }

    #[test]
    fn a_red_gate_holds_even_on_a_recorded_pass() {
        // `gate_green` is present tense: a PASS recorded when the gate was
        // green says nothing about a gate that has since gone red.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);
        let req = MergeRequest {
            gate_green: false,
            ..request(false)
        };

        let exec = execute_merge(&database, &mut gh_runner, &req).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::Review(HoldReason::GateNotGreen))
        );
    }

    #[test]
    fn a_high_risk_class_holds_even_with_a_pass() {
        let database = db();
        let outcome = ReviewOutcome {
            risk_class: Some(RiskClass::Security),
            ..pass_outcome()
        };
        // Recorded with a matching dispatch class so the *only* thing holding
        // it is the class being high-risk.
        let decision = decide_merge(true, "security", outcome.judgment());
        record_review(
            &database,
            &ReviewRecord {
                issue: issue(),
                pr_number: 7,
                dispatch_class: "security".into(),
                head_sha: Some(HEAD.into()),
                base_branch: Some("main".into()),
                outcome,
                decision,
            },
        )
        .unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::Review(HoldReason::HighRiskClass {
                class: RiskClass::Security
            }))
        );
    }

    // ── freshness guards ────────────────────────────────────────────────

    #[test]
    fn a_review_that_moved_on_is_stale_and_holds() {
        // Goal 5: no change reaches the default branch without a reviewer
        // having read *it*.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", OTHER_HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::ReviewIsStale {
                reviewed_head: HEAD.into(),
                current_head: OTHER_HEAD.into(),
            })
        );
    }

    #[test]
    fn a_review_with_no_recorded_head_cannot_authorize_a_merge() {
        // Every review recorded before rung 6 is in this state, and "we don't
        // know what was reviewed" must not read as "nothing changed".
        let database = db();
        store_review(&database, 7, None, pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(
            matches!(
                exec.outcome.hold(),
                Some(MergeHold::ReviewHeadUnknown { .. })
            ),
            "{:?}",
            exec.outcome
        );
    }

    #[test]
    fn a_retargeted_pr_holds_because_the_reviewed_diff_is_not_the_one_landing() {
        // The review read `develop..head`; the PR now targets `main`, so the
        // diff that would land is one no reviewer has seen.
        let database = db();
        store_review_against(&database, 7, Some(HEAD), Some("develop"), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)), // baseRefName is "main"
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::BaseBranchMismatch {
                reviewed_base: "develop".into(),
                current_base: "main".into(),
            })
        );
    }

    #[test]
    fn a_review_with_no_recorded_base_skips_the_comparison_rather_than_inventing_one() {
        // Every review recorded before the field existed: unknown is not a
        // mismatch, and the head-SHA guard still protects the diff.
        let database = db();
        store_review_against(&database, 7, Some(HEAD), None, pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.merged(), "{:?}", exec.outcome);
    }

    #[test]
    fn head_comparison_is_case_insensitive() {
        let database = db();
        store_review(&database, 7, Some(&HEAD.to_uppercase()), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.merged(), "{:?}", exec.outcome);
    }

    // ── PR-state guards ─────────────────────────────────────────────────

    #[test]
    fn an_already_merged_pr_is_stood_down_from_rather_than_held() {
        // A hold says "this PR is still open and waiting on somebody". Said
        // of a merged PR it produced three false sentences in two — "was not
        // merged", "stays open", "labeled for a human" — and parked the
        // issue in `agent:blocked`, the one label that auto-resumes, which
        // re-queues work that has already landed.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("MERGED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome,
            MergeOutcome::AlreadyMerged {
                head_sha: HEAD.into()
            }
        );
        assert!(
            !exec.outcome.merged(),
            "Autopilot did not merge it, and the record must not say it did"
        );
        assert!(exec.outcome.landed());
        let plan = exec.label_plan.expect("a landed PR clears the labels");
        assert!(plan.add.is_empty(), "no agent:* label survives a merge");
        assert_eq!(plan.remove, vec!["agent:ready".to_string()]);
    }

    #[test]
    fn the_already_merged_comment_does_not_claim_the_pr_is_open() {
        let body = render_already_merged_comment(&request(false), true);
        assert!(body.contains("already merged"));
        assert!(!body.contains("was not merged"));
        assert!(!body.contains("stays open"));
        assert!(!body.contains("agent:blocked"));
    }

    #[test]
    fn the_already_merged_comment_does_not_claim_a_label_clear_that_failed() {
        // Written before the clear was attempted, this sentence stood as a
        // falsehood on the issue whenever `gh issue edit` refused — and the
        // record dedup guaranteed no later run would correct it.
        let cleared = render_already_merged_comment(&request(false), true);
        assert!(cleared.contains("has been cleared"), "{cleared}");

        let failed = render_already_merged_comment(&request(false), false);
        assert!(
            !failed.contains("has been cleared"),
            "must not claim a clear that did not happen: {failed}"
        );
        assert!(failed.contains("could **not** be cleared"), "{failed}");
        assert!(
            failed.contains("picked up again"),
            "and must say what that means for the human: {failed}"
        );
    }

    #[test]
    fn an_already_merged_pr_clears_its_labels_before_it_says_it_did() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("MERGED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.commented);
        let label_call = gh_runner
            .seen
            .iter()
            .position(|a| a.contains(&"edit".to_string()))
            .expect("the labels must be edited");
        let comment_call = gh_runner
            .seen
            .iter()
            .position(|a| a.contains(&"comment".to_string()))
            .expect("the issue must be commented on");
        assert!(
            label_call < comment_call,
            "the clear must precede the sentence describing it: {:?}",
            gh_runner.seen
        );
    }

    #[test]
    fn a_closed_but_unmerged_pr_is_still_a_hold() {
        // The distinction is merged-versus-not, not open-versus-not: a PR
        // somebody closed without merging genuinely does await a human.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("CLOSED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::PrNotOpen {
                state: "CLOSED".into()
            })
        );
        let plan = exec.label_plan.expect("a hold labels the issue");
        assert_eq!(plan.add, vec!["agent:blocked".to_string()]);
    }

    #[test]
    fn a_draft_pr_holds() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&format!(
                r#"{{"state":"OPEN","isDraft":true,"mergeable":"MERGEABLE",
                    "mergeStateStatus":"CLEAN","baseRefName":"main",
                    "headRefName":"h","headRefOid":"{HEAD}","url":"u"}}"#
            ))),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(exec.outcome.hold(), Some(&MergeHold::PrIsDraft));
    }

    #[test]
    fn a_conflicting_pr_holds() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&format!(
                r#"{{"state":"OPEN","isDraft":false,"mergeable":"CONFLICTING",
                    "mergeStateStatus":"DIRTY","baseRefName":"main",
                    "headRefName":"h","headRefOid":"{HEAD}","url":"u"}}"#
            ))),
            // the protection lookups now precede the mergeability guard
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(
            matches!(exec.outcome.hold(), Some(MergeHold::PrNotMergeable { .. })),
            "{:?}",
            exec.outcome
        );
    }

    #[test]
    fn an_unknown_merge_state_holds_because_not_yet_computed_is_not_yes() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&format!(
                r#"{{"state":"OPEN","isDraft":false,"mergeable":"UNKNOWN",
                    "mergeStateStatus":"UNKNOWN","baseRefName":"main",
                    "headRefName":"h","headRefOid":"{HEAD}","url":"u"}}"#
            ))),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(matches!(
            exec.outcome.hold(),
            Some(MergeHold::PrNotMergeable { .. })
        ));
    }

    #[test]
    fn an_unstable_but_mergeable_pr_still_merges() {
        // UNSTABLE = a non-required check is red. GitHub will merge it, and
        // refusing would make Autopilot useless in any repo with an optional
        // check.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&format!(
                r#"{{"state":"OPEN","isDraft":false,"mergeable":"MERGEABLE",
                    "mergeStateStatus":"UNSTABLE","baseRefName":"main",
                    "headRefName":"h","headRefOid":"{HEAD}","url":"u"}}"#
            ))),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.merged(), "{:?}", exec.outcome);
    }

    // ── branch protection ───────────────────────────────────────────────

    #[test]
    fn a_branch_requiring_a_human_approval_holds() {
        // The ladder's open question, answered by asking GitHub rather than
        // by assuming. This is this very repository's configuration.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::HumanApprovalRequired {
                base: "main".into(),
                required_approving_review_count: 1,
                require_code_owner_reviews: false,
                enforce_admins: true,
                review_decision: "REVIEW_REQUIRED".into(),
            })
        );
        assert!(
            !gh_runner
                .seen
                .iter()
                .any(|a| a.contains(&"merge".to_string())),
            "no merge may be attempted against a protected branch"
        );
        let plan = exec.label_plan.expect("a hold labels the issue");
        assert_eq!(plan.add, vec!["agent:blocked".to_string()]);
        assert!(exec.commented, "the human must be notified");
    }

    #[test]
    fn an_approved_pr_on_a_protected_branch_merges() {
        // The rung's headline claim, and the one that did not work. Branch
        // protection describes the *branch*: it still says "an approving
        // review is required" after one has been given, so reading only the
        // rule refused an approved PR forever — on a repo with required
        // reviews, which is this one, rung 6 could never merge anything.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_reviewed("OPEN", HEAD, "APPROVED")),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.merged(), "{:?}", exec.outcome);
    }

    #[test]
    fn an_unapproved_pr_is_told_it_needs_an_approval_not_that_it_is_unmergeable() {
        // GitHub reports `mergeStateStatus: BLOCKED` for a PR whose only
        // problem is the missing approval. Checking mergeability first made
        // the protection guard unreachable in exactly the configuration it
        // exists for, and handed the human a symptom instead of a cause.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&format!(
                r#"{{"state":"OPEN","isDraft":false,"mergeable":"MERGEABLE",
                    "mergeStateStatus":"BLOCKED","baseRefName":"main",
                    "headRefName":"h","headRefOid":"{HEAD}",
                    "reviewDecision":"REVIEW_REQUIRED","url":"u"}}"#
            ))),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(
            matches!(
                exec.outcome.hold(),
                Some(MergeHold::HumanApprovalRequired { .. })
            ),
            "{:?}",
            exec.outcome
        );
        let summary = exec.outcome.hold().unwrap().summary();
        assert!(
            summary.contains("main requires 1 approving review"),
            "{summary}"
        );
        assert!(
            summary.contains("REVIEW_REQUIRED"),
            "the human is told what GitHub currently says: {summary}"
        );
    }

    #[test]
    fn an_approved_pr_that_github_still_blocks_is_held_on_the_real_reason() {
        // Approval satisfies the protection guard but is not a merge: a
        // required check that is still red must hold, on its own reason.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&format!(
                r#"{{"state":"OPEN","isDraft":false,"mergeable":"MERGEABLE",
                    "mergeStateStatus":"BLOCKED","baseRefName":"main",
                    "headRefName":"h","headRefOid":"{HEAD}",
                    "reviewDecision":"APPROVED","url":"u"}}"#
            ))),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(
            matches!(exec.outcome.hold(), Some(MergeHold::PrNotMergeable { .. })),
            "{:?}",
            exec.outcome
        );
    }

    #[test]
    fn unreadable_protection_holds_rather_than_reading_as_unprotected() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::failed("", "HTTP 403: Forbidden")),
            // the rules endpoint needs no admin and is still consulted, but
            // finding no ruleset cannot vouch for classic protection we were
            // not allowed to read
            Ok(no_rules()),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(
            matches!(
                exec.outcome.hold(),
                Some(MergeHold::ProtectionUnknown { .. })
            ),
            "{:?}",
            exec.outcome
        );
    }

    #[test]
    fn a_refused_merge_command_becomes_a_hold_not_an_error() {
        // GitHub refusing is information for a human, not a crash: an `Err`
        // here would skip the label and the comment the spec requires.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::failed("", "Pull request is not mergeable")),
            // the re-read that decides whether the failure was pre- or
            // post-mutation: still OPEN, so it was pre-mutation
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(
            matches!(
                exec.outcome.hold(),
                Some(MergeHold::MergeCommandFailed { .. })
            ),
            "{:?}",
            exec.outcome
        );
        assert!(exec.commented);
    }

    #[test]
    fn a_merge_command_that_failed_after_the_merge_landed_is_reported_as_a_merge() {
        // `gh pr merge` merges and *then* deletes the head branch, so a
        // refused deletion exits non-zero on a PR that is merged. Believing
        // the exit code told a human "PR #7 was not merged" about a merged
        // PR and parked the issue in `agent:blocked`, which auto-resumes.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::failed(
                "",
                "failed to delete remote branch autopilot/owner-repo-42: HTTP 403",
            )),
            // the re-read: GitHub says it is merged, and that outranks the
            // exit code
            Ok(pr_view_ok("MERGED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.merged(), "{:?}", exec.outcome);
        assert!(!exec.commented, "a merge posts no 'was not merged' comment");
        let plan = exec.label_plan.expect("a merge clears labels");
        assert_eq!(plan.remove, vec!["agent:ready".to_string()]);
    }

    #[test]
    fn a_merge_command_failure_whose_re_read_also_fails_still_holds() {
        // The re-read answers nothing, so the merge command's own failure is
        // the most useful thing to report — and it must not become an `Err`
        // that skips the label and the comment.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::failed("", "HTTP 502")),
            Ok(GhOutput::failed("", "HTTP 502")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(
            matches!(
                exec.outcome.hold(),
                Some(MergeHold::MergeResultUnknown { .. })
            ),
            "an unanswered re-read is not a definite 'not merged': {:?}",
            exec.outcome
        );
        let summary = exec.outcome.hold().unwrap().summary();
        assert!(summary.contains("unknown"), "{summary}");
        assert!(exec.commented);
    }

    // ── storage ─────────────────────────────────────────────────────────

    #[test]
    fn a_repeated_hold_appends_no_second_record_and_names_the_first() {
        // Merge records share the issue entity with reviews. A hold that
        // never resolves is polled indefinitely, and appending an identical
        // record every pass grew the trail without adding a fact — until, at
        // the traversal cap, the reviews fell out of the window and this
        // module read "never reviewed" and held the PR forever. The
        // per-predicate budget makes that unreachable; not writing the
        // repeat at all is why the trail stays readable.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let first = {
            let mut gh_runner = ScriptedGh::new(vec![
                Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
                Ok(protected_requiring_one_approval()),
                Ok(GhOutput::ok(r#"{"labels":[]}"#)),
                Ok(GhOutput::ok("")),
                Ok(label_created()),
                Ok(GhOutput::ok("")),
            ]);
            execute_merge(&database, &mut gh_runner, &request(false)).unwrap()
        };
        assert!(first.record_appended);

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:blocked"}]}"#)),
        ]);
        let second = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(!second.record_appended, "the repeat states nothing new");
        assert_eq!(
            second.record_drawer_id, first.record_drawer_id,
            "the report still names the record that says this"
        );
        assert_eq!(merges_for_issue(&database, &issue()).unwrap().len(), 1);
    }

    #[test]
    fn a_hold_that_changed_is_a_new_fact_and_is_recorded() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);
        execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", OTHER_HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);
        let second = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(second.record_appended);
        let records = merges_for_issue(&database, &issue()).unwrap();
        assert_eq!(records.len(), 2, "two different holds are two facts");
        assert!(records.iter().all(|r| !r.outcome.merged()));
    }

    #[test]
    fn a_rehearsal_and_a_real_attempt_keep_separate_histories() {
        // A dry run notifies nobody, so it must never suppress a real
        // comment — nor be suppressed by one.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        for dry in [true, true, false] {
            let mut responses = vec![
                Ok(pr_view_ok("OPEN", HEAD)),
                Ok(unprotected()),
                Ok(no_rules()),
            ];
            if !dry {
                responses.push(Ok(GhOutput::ok("merged")));
                responses.push(Ok(GhOutput::ok(r#"{"labels":[]}"#)));
            }
            let mut gh_runner = ScriptedGh::new(responses);
            execute_merge(&database, &mut gh_runner, &request(dry)).unwrap();
        }

        let records = merges_for_issue(&database, &issue()).unwrap();
        assert_eq!(
            records.len(),
            2,
            "the repeated rehearsal collapses; the real merge does not: {records:?}"
        );
        assert!(records[0].dry_run && !records[1].dry_run);
    }

    #[test]
    fn the_same_hold_is_not_commented_on_twice() {
        // `HumanApprovalRequired` on a protected branch never resolves on its
        // own, so a poll loop would bury the issue in identical comments —
        // the shape `exhaust_issue` already refuses.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let first = {
            let mut gh_runner = ScriptedGh::new(vec![
                Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
                Ok(protected_requiring_one_approval()),
                Ok(GhOutput::ok(r#"{"labels":[]}"#)),
                Ok(GhOutput::ok("")),
                Ok(label_created()),
                Ok(GhOutput::ok("")),
            ]);
            execute_merge(&database, &mut gh_runner, &request(false)).unwrap()
        };
        assert!(first.commented, "the first hold must notify the human");

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:blocked"}]}"#)),
        ]);
        let second = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(!second.commented, "the same hold says nothing new");
        assert!(
            !gh_runner
                .seen
                .iter()
                .any(|a| a.contains(&"comment".to_string())),
            "no comment may be posted: {:?}",
            gh_runner.seen
        );
        assert!(
            !second.record_drawer_id.is_empty(),
            "the report still names the record that already says this"
        );
    }

    #[test]
    fn a_comment_that_failed_to_post_does_not_suppress_the_next_one() {
        // The record is what suppresses the next run's comment, so writing
        // it before the comment meant a failed post left a record claiming
        // the hold had been reported — and the human was never told, on that
        // run or any later one, because the reason had not changed.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::failed("", "HTTP 403: Forbidden")),
        ]);
        let err = execute_merge(&database, &mut gh_runner, &request(false)).unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");
        assert!(
            merges_for_issue(&database, &issue()).unwrap().is_empty(),
            "an unsent comment leaves no record claiming it was sent"
        );

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);
        let second = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(second.commented, "the human must still be told");
    }

    #[test]
    fn a_hold_on_an_exhausted_issue_leaves_the_stop_sign_up() {
        // `agent:exhausted` never self-resumes, per the spec. But
        // `set_exclusive_label` removes every *other* `agent:*` label, so
        // applying `agent:blocked` unconditionally moved an exhausted issue
        // to the one label that *does* auto-resume — Autopilot clearing its
        // own permanent stop sign.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_reviewed("OPEN", HEAD, "REVIEW_REQUIRED")),
            Ok(protected_requiring_one_approval()),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:exhausted"}]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.hold().is_some());
        assert!(
            exec.label_plan.is_none(),
            "no label transition at all: {:?}",
            exec.label_plan
        );
        assert!(
            !gh_runner
                .seen
                .iter()
                .any(|a| a.contains(&"--add-label".to_string())),
            "nothing may be added over agent:exhausted: {:?}",
            gh_runner.seen
        );
        assert!(exec.commented, "the attempt is still explained");
    }

    #[test]
    fn a_hold_that_changed_is_commented_on_again() {
        // Suppressing a *different* reason would hide new information.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("MERGED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);
        execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", OTHER_HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);
        let second = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(matches!(
            second.outcome.hold(),
            Some(MergeHold::ReviewIsStale { .. })
        ));
        assert!(second.commented, "a new reason is new information");
    }

    #[test]
    fn a_merge_is_recorded_even_when_the_label_write_fails() {
        // The merge already happened and cannot be undone; losing the audit
        // record of the only irreversible action would be the worse half.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::failed("", "HTTP 403: Forbidden")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(
            exec.outcome.merged(),
            "a merge that happened must be reported as one: {:?}",
            exec.outcome
        );
        let label_error = exec.label_error.expect("the label failure is reported");
        assert!(label_error.contains("403"), "{label_error}");
        assert!(exec.label_plan.is_none());

        let records = merges_for_issue(&database, &issue()).unwrap();
        assert_eq!(records.len(), 1, "the merge must still be on the record");
        assert!(records[0].outcome.merged());
    }

    #[test]
    fn a_dry_run_is_recorded_as_a_dry_run() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
        ]);
        execute_merge(&database, &mut gh_runner, &request(true)).unwrap();

        let records = merges_for_issue(&database, &issue()).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].dry_run, "a rehearsal must not read as a merge");
        assert!(!records[0].outcome.merged());
    }

    #[test]
    fn the_json_shape_puts_the_outcome_tag_at_the_top_level() {
        // The defect the previous review pass fixed for `ExhaustExecution`,
        // with a regression test, and that was applied to one twin and not
        // the other: without the flatten, `merge --json` emits
        // `{"outcome":{"outcome":"merged",…}}`.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
        ]);
        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        let json = serde_json::to_value(&exec).unwrap();
        assert_eq!(json["outcome"], "merged");
        assert_eq!(json["strategy"], "squash");
        assert_eq!(json["head_sha"], HEAD);
        assert!(
            json["outcome"]["outcome"].is_null(),
            "no doubly-nested tag: {json}"
        );
    }

    #[test]
    fn an_issue_with_no_merges_reads_back_empty() {
        let database = db();
        assert!(merges_for_issue(&database, &issue()).unwrap().is_empty());
    }

    #[test]
    fn a_closed_pr_is_reported_closed_even_when_nothing_was_ever_reviewed() {
        // The open check sits above the storage guards precisely so that
        // `NotReviewed` — whose comment says "the pull request stays open" —
        // cannot be reached on a PR somebody closed.
        let database = db();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("CLOSED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::PrNotOpen {
                state: "CLOSED".into()
            }),
            "a closed PR's state outranks the missing review: {:?}",
            exec.outcome
        );
    }

    #[test]
    fn a_closed_pr_is_reported_closed_even_when_the_gate_is_red() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("CLOSED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(
            &database,
            &mut gh_runner,
            &MergeRequest {
                gate_green: false,
                ..request(false)
            },
        )
        .unwrap();

        assert!(
            matches!(exec.outcome.hold(), Some(MergeHold::PrNotOpen { .. })),
            "{:?}",
            exec.outcome
        );
    }

    #[test]
    fn every_hold_that_claims_the_pr_is_open_is_only_reachable_on_an_open_pr() {
        // The positive list in `pr_definitely_still_open` is only sound if
        // `evaluate` cannot produce those holds for a non-open PR. The open
        // guard's position is what makes that true, so pin the pairing.
        for hold in [
            MergeHold::NotReviewed,
            MergeHold::Review(HoldReason::GateNotGreen),
            MergeHold::PrIsDraft,
        ] {
            assert!(
                hold.pr_definitely_still_open(),
                "{hold:?} claims the PR is open"
            );
        }
        for hold in [
            MergeHold::PrNotOpen {
                state: "CLOSED".into(),
            },
            MergeHold::MergeResultUnknown {
                detail: String::new(),
                read_failure: String::new(),
            },
        ] {
            assert!(!hold.pr_definitely_still_open(), "{hold:?}");
        }
    }

    #[test]
    fn no_hold_comment_claims_the_pr_is_open_while_saying_it_is_closed() {
        // The exclusion-list version of `pr_definitely_still_open` printed
        // "The pull request stays open" three lines under "the PR is CLOSED,
        // not open".
        for hold in [
            MergeHold::PrNotOpen {
                state: "CLOSED".into(),
            },
            MergeHold::MergeResultUnknown {
                detail: "exit 1".into(),
                read_failure: "HTTP 502".into(),
            },
        ] {
            let body = render_hold_comment(&request(false), &hold, AgentLabel::Blocked);
            assert!(
                !body.contains("stays open"),
                "{hold:?} must not claim the PR is open:\n{body}"
            );
        }
        // …while a hold that genuinely leaves it open still says so.
        let body = render_hold_comment(&request(false), &MergeHold::PrIsDraft, AgentLabel::Blocked);
        assert!(body.contains("stays open"), "{body}");
    }

    #[test]
    fn a_merge_command_failure_on_a_concurrently_closed_pr_reports_it_closed() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(unprotected()),
            Ok(no_rules()),
            Ok(GhOutput::failed("", "Pull request is not mergeable")),
            // a human closed it while the merge was being attempted
            Ok(pr_view_ok("CLOSED", HEAD)),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
            Ok(label_created()),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::PrNotOpen {
                state: "CLOSED".into()
            }),
            "a re-read that says CLOSED is not 'the merge simply failed'"
        );
    }

    #[test]
    fn the_hold_comment_names_the_reason_and_the_way_back() {
        let body = render_hold_comment(
            &request(false),
            &MergeHold::HumanApprovalRequired {
                base: "main".into(),
                required_approving_review_count: 1,
                require_code_owner_reviews: false,
                enforce_admins: true,
                review_decision: "REVIEW_REQUIRED".into(),
            },
            AgentLabel::Blocked,
        );
        assert!(body.contains("was not merged"));
        assert!(body.contains("main requires 1 approving review"));
        assert!(
            !render_hold_comment(
                &request(false),
                &MergeHold::HumanApprovalRequired {
                    base: "main".into(),
                    required_approving_review_count: 0,
                    require_code_owner_reviews: true,
                    enforce_admins: true,
                    review_decision: "REVIEW_REQUIRED".into(),
                },
                AgentLabel::Blocked,
            )
            .contains("0 approving review"),
            "a code-owner requirement must not read to a human as \"requires 0\""
        );
        assert!(body.contains("administrators"));
        assert!(body.contains("agent:blocked"));
        assert!(body.contains("agent:ready"));
    }

    #[test]
    fn every_hold_renders_a_nonempty_summary() {
        // A hold a human cannot read is a hold that gets ignored.
        let holds = vec![
            MergeHold::Review(HoldReason::NeedsChanges),
            // The payload-carrying `HoldReason`s specifically: these are what
            // rendered as Rust debug output before `HoldReason::summary`
            // existed, and the `{` assertion below only catches them if they
            // are actually exercised.
            MergeHold::Review(HoldReason::HighRiskClass {
                class: RiskClass::Logic,
            }),
            MergeHold::Review(HoldReason::ClassMismatch {
                dispatch_class: "documentation".into(),
                diff_class: RiskClass::Logic,
            }),
            MergeHold::Review(HoldReason::GateNotGreen),
            MergeHold::Review(HoldReason::ReviewerDidNotRun),
            MergeHold::Review(HoldReason::NoVerdict),
            MergeHold::NotReviewed,
            MergeHold::ReviewHeadUnknown {
                reviewed_at: "2026-08-31T00:00:00Z".into(),
            },
            MergeHold::ReviewIsStale {
                reviewed_head: HEAD.into(),
                current_head: OTHER_HEAD.into(),
            },
            MergeHold::PrNotOpen {
                state: "CLOSED".into(),
            },
            MergeHold::PrIsDraft,
            MergeHold::PrNotMergeable {
                mergeable: "CONFLICTING".into(),
                merge_state_status: "DIRTY".into(),
            },
            MergeHold::BaseBranchMismatch {
                reviewed_base: "main".into(),
                current_base: "develop".into(),
            },
            MergeHold::HumanApprovalRequired {
                base: "main".into(),
                required_approving_review_count: 2,
                require_code_owner_reviews: false,
                enforce_admins: false,
                review_decision: "REVIEW_REQUIRED".into(),
            },
            MergeHold::HumanApprovalRequired {
                base: "main".into(),
                required_approving_review_count: 0,
                require_code_owner_reviews: true,
                enforce_admins: true,
                review_decision: "REVIEW_REQUIRED".into(),
            },
            MergeHold::ProtectionUnknown {
                detail: "403".into(),
            },
            MergeHold::MergeCommandFailed {
                detail: "boom".into(),
            },
        ];
        for hold in holds {
            let summary = hold.summary();
            assert!(!summary.trim().is_empty(), "{hold:?} rendered nothing");
            assert!(
                !summary.contains("{"),
                "{hold:?} left a format placeholder: {summary}"
            );
        }
    }

    #[test]
    fn a_stale_review_summary_shows_both_short_shas() {
        let summary = MergeHold::ReviewIsStale {
            reviewed_head: HEAD.into(),
            current_head: OTHER_HEAD.into(),
        }
        .summary();
        assert!(summary.contains("11111111"));
        assert!(summary.contains("22222222"));
    }
}
