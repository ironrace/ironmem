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
//! | The base branch permits an unapproved merge | The spec's `enforce_admins` case — see below |
//!
//! # Branch protection: the open question rung 6 was handed
//!
//! The design names the Lead as "SOLE merge authority via `gh pr merge`", and
//! the ladder's notes flag that repos whose default branch requires a
//! human-approved review — *this* repository among them, with
//! `enforce_admins` on and therefore no admin bypass — make that claim false.
//! Resolved by asking rather than assuming: [`execute_merge`] reads the base
//! branch's protection rules before merging and, if an approving review is
//! required, holds with [`MergeHold::HumanApprovalRequired`]. Autopilot is
//! not the reviewer of record there and cannot become one; the honest
//! outcome is a labeled PR and a notified human, which is the same terminal
//! state the spec already defines for every non-low-risk change.
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
    /// The PR is not open — already merged, or closed.
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
    /// The base branch requires an approving human review.
    HumanApprovalRequired {
        base: String,
        required_approving_review_count: u64,
        /// A CODEOWNERS approval is required, independent of the count.
        require_code_owner_reviews: bool,
        enforce_admins: bool,
    },
    /// The base branch's protection rules could not be read.
    ProtectionUnknown { detail: String },
    /// `gh pr merge` itself refused.
    MergeCommandFailed { detail: String },
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
                    "{base} requires {requirement}{}, which Autopilot cannot supply",
                    if *enforce_admins {
                        " and enforces the rule on administrators too"
                    } else {
                        ""
                    }
                )
            }
            MergeHold::ProtectionUnknown { detail } => {
                format!("the base branch's protection rules could not be read: {detail}")
            }
            MergeHold::MergeCommandFailed { detail } => format!("`gh pr merge` failed: {detail}"),
        }
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
    /// The PR was not merged, and why.
    Held(MergeHold),
}

impl MergeOutcome {
    pub fn merged(&self) -> bool {
        matches!(self, MergeOutcome::Merged { .. })
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
    pub outcome: MergeOutcome,
    /// The PR as GitHub described it, when we got far enough to ask.
    pub snapshot: Option<PrSnapshot>,
    /// The label transition applied to the issue, if any.
    pub label_plan: Option<labels::LabelPlan>,
    /// Whether a notification comment was posted.
    pub commented: bool,
    /// The drawer id of the appended merge record. Always present: `finish`
    /// records before it does anything else, and a storage failure is an
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
    // 1. The review. Reading storage before touching GitHub means a PR with
    //    no review is refused without a single API call.
    let Some(review) = latest_review_for_pr(db, request.issue, request.pr_number)? else {
        return Ok((MergeOutcome::Held(MergeHold::NotReviewed), None));
    };

    // 2. Rung 5's gate, re-derived against the *present* gate state.
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
        return Ok((MergeOutcome::Held(MergeHold::Review(reason)), None));
    }

    // 3. The PR as GitHub sees it now.
    let snapshot = gh::pr_snapshot(gh_runner, &request.issue.repo, request.pr_number)?;

    if !snapshot.state.eq_ignore_ascii_case("open") {
        let state = snapshot.state.clone();
        return Ok((
            MergeOutcome::Held(MergeHold::PrNotOpen { state }),
            Some(snapshot),
        ));
    }
    if snapshot.is_draft {
        return Ok((MergeOutcome::Held(MergeHold::PrIsDraft), Some(snapshot)));
    }

    // 4. Did the reviewer read *this* commit?
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

    // 5. Is it still the same merge? A retargeted PR was reviewed against a
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

    if !snapshot.mergeable.eq_ignore_ascii_case("mergeable")
        || !mergeable_state_permits(&snapshot.merge_state_status)
    {
        let hold = MergeHold::PrNotMergeable {
            mergeable: snapshot.mergeable.clone(),
            merge_state_status: snapshot.merge_state_status.clone(),
        };
        return Ok((MergeOutcome::Held(hold), Some(snapshot)));
    }

    // 6. Branch protection — the ladder's open question.
    let hold = match gh::branch_protection(gh_runner, &request.issue.repo, &snapshot.base_ref_name)?
    {
        BranchProtection::NoHumanApprovalRequired => None,
        BranchProtection::HumanApprovalRequired {
            required_approving_review_count,
            require_code_owner_reviews,
            enforce_admins,
        } => Some(MergeHold::HumanApprovalRequired {
            base: snapshot.base_ref_name.clone(),
            required_approving_review_count,
            require_code_owner_reviews,
            enforce_admins,
        }),
        BranchProtection::Unknown { detail } => Some(MergeHold::ProtectionUnknown { detail }),
    };
    if let Some(hold) = hold {
        return Ok((MergeOutcome::Held(hold), Some(snapshot)));
    }

    // 7. Every guard passed.
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
        let hold = MergeHold::MergeCommandFailed {
            detail: format!(
                "exit {:?}: {}",
                out.code,
                scrub_and_bound(out.stderr.trim(), MAX_LINEAGE_FIELD_CHARS).text
            ),
        };
        return Ok((MergeOutcome::Held(hold), Some(snapshot)));
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
/// # Why the record is written before the GitHub writes
///
/// By the time `finish` sees a [`MergeOutcome::Merged`] the merge has already
/// happened and cannot be undone. If the record were appended after the label
/// write, a `gh issue edit` that failed — a 403, a rate limit, a dropped
/// connection — would propagate out of `finish` and take the record with it,
/// leaving the one irreversible action Autopilot performs with no entry in the
/// audit trail that exists precisely for it. Recording first costs a
/// never-executed record only when *storage* fails, which is before anything
/// has been written to GitHub at all.
fn finish(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    request: &MergeRequest,
    outcome: MergeOutcome,
    snapshot: Option<PrSnapshot>,
) -> Result<MergeExecution, MemoryError> {
    // Read before the new record is appended, or it would always find itself.
    let hold_already_reported = match (&outcome, request.dry_run) {
        (MergeOutcome::Held(hold), false) => {
            last_reported_hold(db, request.issue, request.pr_number)?.as_ref() == Some(hold)
        }
        _ => false,
    };

    let record_drawer_id = record_merge(
        db,
        &MergeRecord {
            issue: request.issue.clone(),
            pr_number: request.pr_number,
            gate_green: request.gate_green,
            dry_run: request.dry_run,
            outcome: outcome.clone(),
            base_branch: snapshot.as_ref().map(|s| s.base_ref_name.clone()),
            head_sha: snapshot.as_ref().map(|s| s.head_ref_oid.clone()),
        },
    )?;

    let mut commented = false;
    let mut label_plan = None;

    if !request.dry_run {
        match &outcome {
            MergeOutcome::Merged { .. } => {
                // Clear every `agent:*` label. A merged issue that keeps
                // `agent:ready` is re-picked by the next poll forever — the
                // same budget livelock the spec's stagnation control exists
                // to prevent, arrived at from the opposite direction.
                label_plan = Some(labels::set_exclusive_label(gh_runner, request.issue, None)?);
            }
            MergeOutcome::Held(hold) => {
                // Not re-posted when the previous attempt on this PR held for
                // the *same* reason: `HumanApprovalRequired` on a protected
                // branch never resolves on its own, so a poll loop would
                // otherwise bury the issue in identical comments — the shape
                // `exhaust_issue` already refuses. A hold that has *changed*
                // is new information and is still commented on.
                if !hold_already_reported {
                    gh::comment_on_issue(
                        gh_runner,
                        request.issue,
                        &render_hold_comment(request, hold),
                    )?;
                    commented = true;
                }
                // `agent:blocked` and not a fourth label: a held PR is
                // "awaiting a human", which is exactly what the label means,
                // and its auto-resume-on-a-newer-human-comment semantics are
                // the right ones — a human who approves or comments has
                // supplied the thing that was missing. The spec defines three
                // labels and inventing a fourth here would put a state in the
                // repo that nothing else in the design knows how to clear.
                label_plan = Some(labels::set_exclusive_label(
                    gh_runner,
                    request.issue,
                    Some(AgentLabel::Blocked),
                )?);
            }
            MergeOutcome::WouldMerge { .. } => {}
        }
    }

    Ok(MergeExecution {
        issue: request.issue.clone(),
        pr_number: request.pr_number,
        outcome,
        snapshot,
        label_plan,
        commented,
        record_drawer_id,
    })
}

/// The comment posted on a held PR's issue.
///
/// Pure, so the exact text a human sees is asserted in tests rather than
/// discovered in production. Scrubbed and bounded because a hold reason can
/// quote `gh`'s stderr and a review's reason, both of which quote the diff.
pub fn render_hold_comment(request: &MergeRequest, hold: &MergeHold) -> String {
    let body = format!(
        "**Autopilot: PR #{pr} was not merged.**\n\n\
{summary}.\n\n\
The pull request stays open and is labeled `{label}` for a human. Autopilot \
will not merge it on its own; re-labeling the issue `{ready}` after resolving \
the above puts it back in the queue.\n\n\
<sub>Gate reported {gate} at merge time. Autopilot rung 6.</sub>",
        pr = request.pr_number,
        summary = hold.summary(),
        label = AgentLabel::Blocked.as_str(),
        ready = AgentLabel::Ready.as_str(),
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
    pub pr_number: u64,
    pub gate_green: bool,
    pub dry_run: bool,
    pub outcome: MergeOutcome,
    pub head_sha: Option<String>,
    pub recorded_at: String,
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

    let triples = kg.query_entity_current(&entity.id, MAX_ISSUE_EDGES)?;
    let mut records = Vec::new();
    for triple in triples {
        if triple.predicate != HAS_MERGE_PREDICATE {
            continue;
        }
        let Some(object_entity) = kg.get_entity(&triple.object)? else {
            continue;
        };
        let Some(drawer) = db.get_drawer(&object_entity.name)? else {
            continue;
        };
        let body: MergeBody = serde_json::from_str(&drawer.content)?;
        records.push(RecordedMergeSummary {
            pr_number: body.pr_number,
            gate_green: body.gate_green,
            dry_run: body.dry_run,
            outcome: body.outcome,
            head_sha: body.head_sha,
            recorded_at: body.recorded_at,
        });
    }
    records.sort_by(|a, b| a.recorded_at.cmp(&b.recorded_at));
    Ok(records)
}

/// The hold the most recent *executed* attempt on this PR reported, if that
/// attempt was a hold at all.
///
/// Dry runs are skipped because a rehearsal notifies nobody: suppressing a
/// real comment on the strength of one would lose the notification entirely.
/// A later [`MergeOutcome::Merged`] or a *different* hold resets this to
/// `None`/a different value, so the next hold is commented on again.
fn last_reported_hold(
    db: &Database,
    issue: &IssueRef,
    pr_number: u64,
) -> Result<Option<MergeHold>, MemoryError> {
    let records = merges_for_issue(db, issue)?;
    let Some(latest) = records
        .into_iter()
        .rfind(|r| r.pr_number == pr_number && !r.dry_run)
    else {
        return Ok(None);
    };
    match latest.outcome {
        MergeOutcome::Held(hold) => Ok(Some(hold)),
        _ => Ok(None),
    }
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
    Ok(reviews.into_iter().rfind(|r| r.pr_number == pr_number))
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
        GhOutput::ok(&format!(
            r#"{{"state":"{state}","isDraft":false,"mergeable":"MERGEABLE",
                "mergeStateStatus":"CLEAN","baseRefName":"main",
                "headRefName":"autopilot/owner-repo-42","headRefOid":"{head}",
                "url":"https://github.com/owner/repo/pull/7"}}"#
        ))
    }

    fn unprotected() -> GhOutput {
        GhOutput::failed("", "gh: Branch not protected (HTTP 404)")
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
            Ok(GhOutput::ok("merged")),
            // clearing the labels: read, then edit
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.merged(), "{:?}", exec.outcome);
        let merge_argv = &gh_runner.seen[2];
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
        let mut gh_runner = ScriptedGh::new(vec![Ok(pr_view_ok("OPEN", HEAD)), Ok(unprotected())]);

        let exec = execute_merge(&database, &mut gh_runner, &request(true)).unwrap();

        assert!(
            matches!(exec.outcome, MergeOutcome::WouldMerge { .. }),
            "{:?}",
            exec.outcome
        );
        assert_eq!(gh_runner.seen.len(), 2, "only the two reads happened");
        assert!(!exec.commented);
        assert!(exec.label_plan.is_none());
    }

    // ── review guards ───────────────────────────────────────────────────

    #[test]
    fn an_unreviewed_pr_is_never_merged_and_costs_no_api_calls() {
        let database = db();
        let mut gh_runner = ScriptedGh::new(vec![
            // the hold's comment, then the label read + edit
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            exec.snapshot.is_none(),
            "GitHub was never asked about the PR"
        );
    }

    #[test]
    fn a_pass_on_a_different_pr_does_not_authorize_this_one() {
        // The whole reason `latest_review_for_pr` filters by PR number.
        let database = db();
        store_review(&database, 99, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert!(exec.outcome.merged(), "{:?}", exec.outcome);
    }

    // ── PR-state guards ─────────────────────────────────────────────────

    #[test]
    fn an_already_merged_pr_is_not_merged_again() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("MERGED", HEAD)),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
        ]);

        let exec = execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        assert_eq!(
            exec.outcome.hold(),
            Some(&MergeHold::PrNotOpen {
                state: "MERGED".into()
            })
        );
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::ok(
                r#"{"required_pull_request_reviews":{"required_approving_review_count":1},
                    "enforce_admins":{"enabled":true}}"#,
            )),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
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
    fn unreadable_protection_holds_rather_than_reading_as_unprotected() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", HEAD)),
            Ok(GhOutput::failed("", "HTTP 403: Forbidden")),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::failed("", "Pull request is not mergeable")),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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

    // ── storage ─────────────────────────────────────────────────────────

    #[test]
    fn every_outcome_is_recorded_and_holds_do_not_overwrite_each_other() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        // First pass comments; the second repeats the same hold and does not.
        for responses in [
            vec![
                Ok(pr_view_ok("MERGED", HEAD)),
                Ok(GhOutput::ok("")),
                Ok(GhOutput::ok(r#"{"labels":[]}"#)),
                Ok(GhOutput::ok("")),
            ],
            vec![
                Ok(pr_view_ok("MERGED", HEAD)),
                Ok(GhOutput::ok(r#"{"labels":[]}"#)),
                Ok(GhOutput::ok("")),
            ],
        ] {
            let mut gh_runner = ScriptedGh::new(responses);
            execute_merge(&database, &mut gh_runner, &request(false)).unwrap();
        }

        let records = merges_for_issue(&database, &issue()).unwrap();
        assert_eq!(
            records.len(),
            2,
            "append-only: two identical holds are two facts"
        );
        assert!(records.iter().all(|r| !r.outcome.merged()));
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
                Ok(pr_view_ok("MERGED", HEAD)),
                Ok(GhOutput::ok("")),
                Ok(GhOutput::ok(r#"{"labels":[]}"#)),
                Ok(GhOutput::ok("")),
            ]);
            execute_merge(&database, &mut gh_runner, &request(false)).unwrap()
        };
        assert!(first.commented, "the first hold must notify the human");

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("MERGED", HEAD)),
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
            "the repeat is still recorded"
        );
    }

    #[test]
    fn a_hold_that_changed_is_commented_on_again() {
        // Suppressing a *different* reason would hide new information.
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("MERGED", HEAD)),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::ok("")),
        ]);
        execute_merge(&database, &mut gh_runner, &request(false)).unwrap();

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(pr_view_ok("OPEN", OTHER_HEAD)),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
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
            Ok(GhOutput::ok("merged")),
            Ok(GhOutput::failed("", "HTTP 403: Forbidden")),
        ]);

        let err = execute_merge(&database, &mut gh_runner, &request(false)).unwrap_err();
        assert!(err.to_string().contains("403"), "{err}");

        let records = merges_for_issue(&database, &issue()).unwrap();
        assert_eq!(records.len(), 1, "the merge must still be on the record");
        assert!(records[0].outcome.merged());
    }

    #[test]
    fn a_dry_run_is_recorded_as_a_dry_run() {
        let database = db();
        store_review(&database, 7, Some(HEAD), pass_outcome());
        let mut gh_runner = ScriptedGh::new(vec![Ok(pr_view_ok("OPEN", HEAD)), Ok(unprotected())]);
        execute_merge(&database, &mut gh_runner, &request(true)).unwrap();

        let records = merges_for_issue(&database, &issue()).unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].dry_run, "a rehearsal must not read as a merge");
        assert!(!records[0].outcome.merged());
    }

    #[test]
    fn an_issue_with_no_merges_reads_back_empty() {
        let database = db();
        assert!(merges_for_issue(&database, &issue()).unwrap().is_empty());
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
            },
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
                },
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
            },
            MergeHold::HumanApprovalRequired {
                base: "main".into(),
                required_approving_review_count: 0,
                require_code_owner_reviews: true,
                enforce_admins: true,
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
