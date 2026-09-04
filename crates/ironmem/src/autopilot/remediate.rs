//! Rung 11 — **the red path.**
//!
//! Rung 10 closed the green half of the spec's data flow: an IC goes green, a
//! reviewer reads the PR, and on `PASS` + low risk + a matching class the PR
//! merges. The other half of the same fork was left drawn but never run:
//!
//! > (b) review diff → PASS | NEEDS CHANGES
//! >     ├─ NEEDS CHANGES ─► re-dispatch IC to fix
//! >     │                    (counts against the same per-issue attempt cap)
//!
//! and its row of the error table:
//!
//! > Reviewer uncertain, or returns NEEDS CHANGES → ⟨r2⟩ Re-dispatch the IC
//! > to fix, counting against the same per-issue attempt cap. On exhaustion
//! > the PR stays open for a human — **never merged with an unresolved
//! > finding.**
//!
//! Until this module, a `needs_changes` verdict was a full stop. Rung 6 held
//! the PR at [`super::review::HoldReason::NeedsChanges`], commented, and
//! flipped the issue to `agent:blocked`; the reviewer's findings sat in a
//! drawer, and the IC that could have acted on them was never told they
//! existed. Every piece of the loop was built and the last arrow was missing,
//! which is the shape rung 10 found in `AlreadySucceeded` and lesson 45 names.
//!
//! # The hazard rung 10 named, and what it actually is
//!
//! Rung 10 recorded its residual as *"re-opening work whose lineage records a
//! success is a distinct capability with its own hazards"*. Stated precisely,
//! there are three, and this module is shaped by them.
//!
//! **One: the guards that make an issue finished are the guards in the way.**
//! [`super::run::run_issue`] returns `AlreadySucceeded` without dispatching,
//! and [`super::queue::plan_queue`] defers on `AlreadySucceeded`, both reading
//! the same `best_verdict`. The tempting fix — clear the success — is wrong
//! twice over: `best_verdict` is deliberately never downgraded (a success is
//! *"best so far"*, not *"most recent"*), and rung 10's own `plan_advance`
//! needs that success to find the PR again after the fix is pushed. So the
//! success stays exactly where it is and this module supplies a *second,
//! narrower* reason to dispatch, which both guards consult by name. Re-opening
//! finished work is then an explicit, recorded, auditable act rather than the
//! erasure of the record that says it was finished.
//!
//! **Two: the gate is already green, so the gate cannot be the goal.** This is
//! the one that would have shipped silently. A remediation dispatch renders
//! the same `/goal` condition every other dispatch renders — *"the approved
//! gate passes"* — and at the reviewed commit it already does. The IC would
//! run the gate, watch it pass, report `met`, push nothing, and the reviewer
//! would read the identical commit and say the identical thing, forever, at
//! full price per turn. [`super::turn_prompt`] therefore **extends the
//! condition** for a remediation dispatch: the gate must pass *and* the
//! findings must be addressed by a commit pushed to the branch. A dispatch
//! that cannot move the head has not done the job, and the prompt says so
//! rather than leaving it to be inferred.
//!
//! **Three: the loop has to be bounded on every axis it can spend on.** There
//! are three, and each already had an owner: dispatches are bounded by the
//! per-issue attempt cap (the spec's own answer, and this module checks it
//! *before* anything else), reviews are bounded by rung 10's head-SHA trigger
//! (no new commit, no new review, so a stuck remediation is never re-billed
//! for a second opinion on the same diff), and the day is bounded by rung 5's
//! ledger. Nothing here adds a fourth kind of spend; it adds a new *reason* to
//! spend on the two that exist.
//!
//! # What is stored, and what is derived
//!
//! An eleventh drawer kind, of kind 2's shape ([`RemediationRecord`],
//! `logical_key` per issue). It holds only facts that cannot be recomputed:
//! which PR and which **commit** the findings are about, the findings
//! themselves, and the lineage depth at the moment it was armed.
//!
//! Everything else is derived, and deliberately so. There is **no `resolved`
//! flag**, because a flag is a write that can be lost and a state that can
//! disagree with the world. A remediation is finished when the issue records a
//! *newer success than the one it was armed against* — that is
//! [`active_remediation`]'s whole test, and it is true exactly when the IC has
//! pushed a fix that went green. A failed remediation dispatch leaves
//! `best_commit_sha` untouched (the no-downgrade rule), so the remediation
//! stays in force and the next tick tries again, up to the cap. This is rung
//! 10's lesson 46 pointed at a different question: *the claim is about a
//! commit, not about a branch or a flag.*
//!
//! # Idempotence, including the failure outcomes
//!
//! [`arm_remediation`] keys on `(pr_number, head_sha)`: one remediation per
//! *reviewed commit*, which is the same dimension rung 10 keys its review
//! trigger on and rung 6 keys its merge guard on. Re-running an advance pass
//! over an issue whose remediation is already in force reports
//! [`ArmOutcome::AlreadyArmed`] and writes nothing — the record would
//! otherwise be rewritten on every pass, resetting the delivery depth and
//! making "has this been dispatched yet?" unanswerable. Rung 9's lesson 41,
//! whose sharper half is that the *failure* outcomes need covering too: a
//! remediation that was armed and dispatched and did not work is still the
//! same remediation, not a new one.
//!
//! # Where it ends
//!
//! At the attempt cap, and only there. [`ArmOutcome::CapReached`] is checked
//! **first**, ahead of the already-armed test, so an issue that has spent its
//! attempts stops being carried by this module even though its remediation
//! record still exists — and rung 6's hold then runs for real, comments, and
//! flips the issue to `agent:blocked`. That is the spec's *"on exhaustion the
//! PR stays open for a human"*, and the ordering is what makes it reachable:
//! the other way round, an armed record would shadow the cap forever and the
//! human would never be told.

use serde::{Deserialize, Serialize};

use super::lineage::{self, AttemptOutcome};
use super::scrub::scrub_and_bound;
use super::{read_current, validate_repo, write_current, IssueRef};
use crate::db::schema::Database;
use crate::error::MemoryError;

/// The longest findings text rendered into a remediation's turn prompt.
///
/// **Derived from a measurement, not chosen.** The spec's ⟨r5-doc⟩ row records
/// that a `/goal` condition may be up to **4,000 characters** — a platform
/// limit, not a preference — and [`super::turn_prompt::render`] emits 1,498
/// characters for a realistic dispatch and 2,570 once the remediation block is
/// added. 1,200 is what fits inside the remainder with headroom left for a
/// longer issue body, and
/// `turn_prompt::tests::a_realistic_remediation_condition_stays_under_the_4000_char_platform_limit`
/// is the guard that keeps the two numbers honest with each other.
///
/// The first draft of this constant was 4,000 — reasoning about the findings
/// in isolation and arriving at a bound that could not bind, because it was
/// the size of the whole budget it had to fit inside. Truncation costs
/// nothing permanent: the reviewer's full text stays in its own drawer, and
/// only the copy pasted into the prompt is bounded.
pub const MAX_FINDINGS_CHARS: usize = 1_200;

/// A reviewer's `needs_changes` verdict, armed for re-dispatch.
///
/// Kind 2's shape: `logical_key` per issue, so an issue has at most one
/// remediation in force and a newer one replaces the older. Two remediations
/// on one issue are never two facts to keep — the older one is about a commit
/// that has been superseded, and the review records themselves are the
/// append-only history of what was said about each.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemediationRecord {
    pub issue: IssueRef,
    /// The PR the reviewer read.
    pub pr_number: u64,
    /// The commit it read. Half of this record's idempotence key, and the
    /// reason a pushed fix arms a *new* remediation rather than re-using this
    /// one: the findings are about this diff and no other.
    pub head_sha: String,
    /// What the reviewer said, scrubbed and bounded.
    ///
    /// `None` when the review recorded no reason. That is not a reason to
    /// refuse the remediation — the verdict alone is a fact worth acting on —
    /// so [`super::turn_prompt`] renders a mechanical instruction with no
    /// quoted findings rather than nothing at all. Rung 9's lesson 35: the
    /// guaranteed half is stored separately from the optional half and they
    /// are composed on read, so the half that always exists cannot be lost
    /// with the half that sometimes does.
    pub findings: Option<String>,
    pub armed_at: String,
    /// The issue's cumulative attempt count when this was armed.
    ///
    /// Rung 7's `redirect_issued_after_attempts`, for the same purpose:
    /// without it, "armed" and "armed and actually dispatched" are
    /// indistinguishable, and every report this module makes would have to
    /// guess which one it was looking at.
    pub armed_after_attempts: u32,
    /// The issue's `best_commit_sha` when this was armed — the commit whose
    /// success this remediation is re-opening.
    ///
    /// [`active_remediation`] compares it against the issue's current best
    /// commit, and a difference means a newer success has landed: the IC
    /// pushed the fix and this remediation is done. `None` on either side
    /// means the comparison cannot be made, which keeps the remediation in
    /// force — the attempt cap still bounds it, and the alternative would
    /// make remediation silently inoperative for any issue whose success
    /// recorded no commit.
    pub armed_at_commit: Option<String>,
}

#[derive(Serialize, Deserialize)]
struct RemediationBody {
    repo: String,
    issue: u64,
    pr_number: u64,
    head_sha: String,
    #[serde(default)]
    findings: Option<String>,
    armed_at: String,
    #[serde(default)]
    armed_after_attempts: u32,
    #[serde(default)]
    armed_at_commit: Option<String>,
}

fn remediation_key(issue: &IssueRef) -> String {
    format!("remediation:{}", issue.slug())
}

/// Write (overwrite) an issue's remediation record.
///
/// `findings` is scrubbed and bounded here rather than by the caller, for the
/// reason [`super::review::record_review`] scrubs its own reason on the write
/// path: the text quotes a diff and can carry whatever the diff carried, and
/// this is the boundary it becomes persisted state at.
pub fn upsert_remediation(
    db: &Database,
    record: &RemediationRecord,
) -> Result<String, MemoryError> {
    validate_repo(&record.issue.repo)?;
    let findings = record
        .findings
        .as_deref()
        .map(|text| scrub_and_bound(text, MAX_FINDINGS_CHARS).text);
    let body = RemediationBody {
        repo: record.issue.repo.clone(),
        issue: record.issue.number,
        pr_number: record.pr_number,
        head_sha: record.head_sha.clone(),
        findings,
        armed_at: record.armed_at.clone(),
        armed_after_attempts: record.armed_after_attempts,
        armed_at_commit: record.armed_at_commit.clone(),
    };
    write_current(
        db,
        &remediation_key(&record.issue),
        &serde_json::to_string(&body)?,
    )
}

/// Read an issue's remediation record, in force or not.
pub fn get_remediation(
    db: &Database,
    issue: &IssueRef,
) -> Result<Option<RemediationRecord>, MemoryError> {
    let Some(drawer) = read_current(db, &remediation_key(issue))? else {
        return Ok(None);
    };
    let body: RemediationBody = serde_json::from_str(&drawer.content)?;
    Ok(Some(RemediationRecord {
        issue: IssueRef::new(body.repo, body.issue),
        pr_number: body.pr_number,
        head_sha: body.head_sha,
        findings: body.findings,
        armed_at: body.armed_at,
        armed_after_attempts: body.armed_after_attempts,
        armed_at_commit: body.armed_at_commit,
    }))
}

/// Drop an issue's remediation record entirely.
///
/// The human override, reached by `ironmem autopilot remediate <repo> <n>
/// --clear`. Deliberately a delete rather than a "resolved" flag: a cleared
/// remediation should leave the issue in exactly the state it would have been
/// in had the reviewer never been asked, and a later review of a later commit
/// should be free to arm a fresh one. A flag would have to be un-set to allow
/// that, and un-setting a flag that means "a human decided" is the shape rung
/// 6's `agent:exhausted` exists to refuse.
///
/// **It stops the Lead, not the next advance pass.** Deleting the record does
/// not delete the review that caused it, and [`super::advance`] arms from that
/// review: while `--remediate` is on, the next pass finds the same
/// `needs_changes` at the same head and calls [`arm_remediation`] again, which
/// arms the same `(pr_number, head_sha)` afresh and resets
/// `armed_after_attempts`. Clearing is therefore a "stop for now", and the
/// operator who wants the PR handed over for good must also drop `--remediate`
/// or take the issue back with `agent:blocked` — the CLI's own output says so.
///
/// Idempotent: clearing an issue with no remediation is `Ok(false)`.
pub fn clear_remediation(db: &Database, issue: &IssueRef) -> Result<bool, MemoryError> {
    let id = super::logical_drawer_id(&remediation_key(issue));
    db.with_transaction(|tx| {
        let deleted = Database::delete_drawer_tx(tx, &id)?;
        Database::wal_log_tx(
            tx,
            "autopilot_clear_remediation",
            &serde_json::json!({
                "drawer_id": &id,
                "issue": issue.canonical(),
                "deleted": deleted,
            }),
            None,
        )?;
        Ok(deleted)
    })
}

/// The remediation currently in force on an issue, if any.
///
/// This is the function [`super::queue::plan_queue`] and
/// [`super::run::run_issue`] consult to decide whether a succeeded issue may
/// none the less be dispatched, and the one [`super::turn_prompt`] reads its
/// findings from. It is therefore the single definition of *"this issue's
/// success has been re-opened"*, and every caller gets the same answer.
///
/// A remediation is in force from the moment it is armed until the issue
/// records a **newer success than the one it was armed against**. Nothing
/// else ends it — not a failed dispatch (which leaves `best_commit_sha`
/// untouched by the no-downgrade rule, so the next tick tries again, which is
/// the spec's own *"counting against the same per-issue attempt cap"*), and
/// not the passage of time. The cap is what ends an unproductive one, and the
/// cap is checked by the callers that can act on it.
pub fn active_remediation(
    db: &Database,
    issue: &IssueRef,
) -> Result<Option<RemediationRecord>, MemoryError> {
    let Some(record) = get_remediation(db, issue)? else {
        return Ok(None);
    };
    let current_commit = lineage::get_issue_status(db, issue)?
        .filter(|status| status.best_verdict == Some(AttemptOutcome::Success))
        .and_then(|status| status.best_commit_sha);

    // A *newer* success than the one this was armed against means the IC
    // pushed a fix and the gate went green on it: the remediation is done,
    // and rung 10's pass will review the new head. Compared
    // case-insensitively, the way `merge::evaluate` and rung 10's gate-green
    // test compare the same kind of value — a differently-cased spelling of
    // one commit is one commit, and reading it as two would keep a finished
    // remediation in force and re-dispatch against findings already fixed.
    //
    // An unknown commit on either side answers "not moved". That keeps the
    // remediation in force, which is the direction the attempt cap already
    // bounds; the inverse would make remediation quietly inoperative for
    // every issue whose success was recorded without a commit sha.
    let superseded = match (record.armed_at_commit.as_deref(), current_commit.as_deref()) {
        (Some(armed), Some(current)) => !armed.eq_ignore_ascii_case(current),
        _ => false,
    };
    Ok((!superseded).then_some(record))
}

/// What [`arm_remediation`] was asked to arm.
#[derive(Debug, Clone)]
pub struct ArmRequest<'a> {
    pub issue: &'a IssueRef,
    pub pr_number: u64,
    /// The commit the reviewer read. Half the idempotence key.
    pub head_sha: &'a str,
    /// The reviewer's recorded reason, or `None` if it gave none.
    pub findings: Option<&'a str>,
    /// The per-issue attempt cap this issue is dispatched under. Passed in
    /// rather than defaulted so it is the *same* value the caller's
    /// `RunConfig`/`QueueConfig` uses; a remediation armed against one cap and
    /// dispatched under another would either never fire or never stop.
    pub attempt_cap: u32,
}

/// What arming did, or why it did nothing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "arm", rename_all = "snake_case")]
pub enum ArmOutcome {
    /// A remediation is now in force for this commit.
    Armed {
        pr_number: u64,
        head_sha: String,
        /// Whether the reviewer gave a reason to pass on. A remediation with
        /// no findings is still armed — the verdict is the fact — but an
        /// operator reading a report should be able to tell the two apart.
        has_findings: bool,
    },
    /// This exact commit is already armed; nothing was written.
    AlreadyArmed {
        pr_number: u64,
        head_sha: String,
        /// Attempts recorded since it was armed. Zero means the Lead has not
        /// dispatched it yet — rung 7's delivery proof, so an operator can
        /// tell "waiting for the next tick" from "tried and did not work".
        dispatches_since: u32,
    },
    /// The issue has spent its attempts. **Nothing is armed, deliberately**:
    /// this is the spec's *"on exhaustion the PR stays open for a human"*, and
    /// the caller's next step is rung 6's real hold, which comments and flips
    /// the issue to `agent:blocked` so a human is actually told.
    CapReached {
        cumulative_attempt_n: u32,
        attempt_cap: u32,
    },
}

impl ArmOutcome {
    /// Whether a remediation is in force as a result of this call.
    ///
    /// True for both `Armed` and `AlreadyArmed`, which is the distinction the
    /// caller acts on: in both cases the Lead will re-dispatch, so the merge
    /// must be rehearsed rather than executed and the issue must keep its
    /// `agent:ready` label. `CapReached` is false — nothing will re-dispatch,
    /// so the PR belongs to a human now.
    pub fn in_force(&self) -> bool {
        matches!(
            self,
            ArmOutcome::Armed { .. } | ArmOutcome::AlreadyArmed { .. }
        )
    }
}

/// Arm a re-dispatch against a reviewer's `needs_changes` verdict.
///
/// # Ordering
///
/// The cap is checked **first**, before the already-armed test, and that order
/// is load-bearing rather than incidental. An issue can reach its cap while a
/// remediation is in force — that is the ordinary way an unproductive
/// remediation ends — and testing "already armed" first would return
/// `AlreadyArmed` forever, so the caller would rehearse the merge on every
/// pass and rung 6's hold would never run for real. The PR would then sit
/// open, unmerged and un-commented, with nobody told: *"never merged with an
/// unresolved finding"* satisfied by accident and *"the PR stays open for a
/// human"* silently unsatisfied.
///
/// Pure enough to test exhaustively: it reads the issue's status and its own
/// record, and writes at most one drawer.
pub fn arm_remediation(db: &Database, request: &ArmRequest) -> Result<ArmOutcome, MemoryError> {
    validate_repo(&request.issue.repo)?;
    let status = lineage::get_issue_status(db, request.issue)?;
    let cumulative_attempt_n = status
        .as_ref()
        .map(|s| s.cumulative_attempt_n)
        .unwrap_or_default();

    if cumulative_attempt_n >= request.attempt_cap {
        return Ok(ArmOutcome::CapReached {
            cumulative_attempt_n,
            attempt_cap: request.attempt_cap,
        });
    }

    let existing = get_remediation(db, request.issue)?;
    if let Some(existing) = existing.filter(|r| {
        r.pr_number == request.pr_number && r.head_sha.eq_ignore_ascii_case(request.head_sha)
    }) {
        return Ok(ArmOutcome::AlreadyArmed {
            pr_number: existing.pr_number,
            head_sha: existing.head_sha,
            // Saturating rather than wrapping: an attempt count that somehow
            // moved backwards is a corrupt read, and reporting a nonsense
            // "4 billion dispatches since" would be worse than reporting none.
            dispatches_since: cumulative_attempt_n.saturating_sub(existing.armed_after_attempts),
        });
    }

    let armed_at_commit = status
        .filter(|s| s.best_verdict == Some(AttemptOutcome::Success))
        .and_then(|s| s.best_commit_sha);
    let has_findings = request.findings.is_some_and(|f| !f.trim().is_empty());
    let record = RemediationRecord {
        issue: request.issue.clone(),
        pr_number: request.pr_number,
        head_sha: request.head_sha.to_string(),
        findings: request
            .findings
            .filter(|f| !f.trim().is_empty())
            .map(str::to_string),
        armed_at: chrono::Utc::now().to_rfc3339(),
        armed_after_attempts: cumulative_attempt_n,
        armed_at_commit,
    };
    upsert_remediation(db, &record)?;
    Ok(ArmOutcome::Armed {
        pr_number: record.pr_number,
        head_sha: record.head_sha,
        has_findings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::lineage::{upsert_issue_status, IssueStatus};

    const REPO: &str = "ironrace/ironmem";
    const HEAD: &str = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    const OTHER_HEAD: &str = "ffffffffffffffffffffffffffffffffffffffff";

    fn issue() -> IssueRef {
        IssueRef::new(REPO, 283)
    }

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// A succeeded issue at `attempts`, green at `commit`.
    fn succeeded(db: &Database, attempts: u32, commit: Option<&str>) {
        upsert_issue_status(
            db,
            &IssueStatus {
                issue: issue(),
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: commit.map(str::to_string),
                cumulative_attempt_n: attempts,
            },
        )
        .unwrap();
    }

    /// Arm against PR 12. The `&issue()` temporary lives for the whole
    /// statement, which is exactly as long as the borrow needs to last.
    fn arm(db: &Database, head: &str, findings: Option<&str>, cap: u32) -> ArmOutcome {
        arm_pr(db, 12, head, findings, cap)
    }

    fn arm_pr(db: &Database, pr: u64, head: &str, findings: Option<&str>, cap: u32) -> ArmOutcome {
        arm_remediation(
            db,
            &ArmRequest {
                issue: &issue(),
                pr_number: pr,
                head_sha: head,
                findings,
                attempt_cap: cap,
            },
        )
        .unwrap()
    }

    #[test]
    fn an_issue_with_no_remediation_has_none_in_force() {
        let db = db();
        assert_eq!(active_remediation(&db, &issue()).unwrap(), None);
        assert_eq!(get_remediation(&db, &issue()).unwrap(), None);
    }

    #[test]
    fn arming_records_the_pr_the_commit_and_the_findings() {
        let db = db();
        succeeded(&db, 2, Some(HEAD));

        let outcome = arm(&db, HEAD, Some("fix the off-by-one"), 5);
        assert_eq!(
            outcome,
            ArmOutcome::Armed {
                pr_number: 12,
                head_sha: HEAD.to_string(),
                has_findings: true,
            }
        );

        let record = active_remediation(&db, &issue()).unwrap().unwrap();
        assert_eq!(record.pr_number, 12);
        assert_eq!(record.head_sha, HEAD);
        assert_eq!(record.findings.as_deref(), Some("fix the off-by-one"));
        assert_eq!(record.armed_after_attempts, 2);
        assert_eq!(record.armed_at_commit.as_deref(), Some(HEAD));
    }

    #[test]
    fn a_review_with_no_reason_still_arms_a_remediation() {
        // The verdict is the fact. Refusing to arm without a reason would let
        // a terse reviewer disable the whole red path.
        let db = db();
        succeeded(&db, 1, Some(HEAD));

        let outcome = arm(&db, HEAD, None, 5);
        assert_eq!(
            outcome,
            ArmOutcome::Armed {
                pr_number: 12,
                head_sha: HEAD.to_string(),
                has_findings: false,
            }
        );
        assert!(active_remediation(&db, &issue()).unwrap().is_some());
    }

    #[test]
    fn whitespace_only_findings_are_stored_as_none() {
        let db = db();
        succeeded(&db, 1, Some(HEAD));
        arm(&db, HEAD, Some("   \n  "), 5);
        assert_eq!(
            active_remediation(&db, &issue()).unwrap().unwrap().findings,
            None,
            "a blank reason must not render an empty 'the reviewer said:' block"
        );
    }

    #[test]
    fn arming_the_same_commit_twice_writes_nothing_new() {
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("first"), 5);

        let second = arm(&db, HEAD, Some("second"), 5);
        assert_eq!(
            second,
            ArmOutcome::AlreadyArmed {
                pr_number: 12,
                head_sha: HEAD.to_string(),
                dispatches_since: 0,
            }
        );
        assert_eq!(
            active_remediation(&db, &issue())
                .unwrap()
                .unwrap()
                .findings
                .as_deref(),
            Some("first"),
            "a second arming must not overwrite the record, or the delivery \
depth resets and 'has this been dispatched?' stops being answerable"
        );
    }

    #[test]
    fn dispatches_since_counts_attempts_recorded_after_arming() {
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);

        // One remediation dispatch ran and failed: the attempt count moves,
        // the success's commit does not.
        succeeded(&db, 3, Some(HEAD));

        assert_eq!(
            arm(&db, HEAD, Some("f"), 5),
            ArmOutcome::AlreadyArmed {
                pr_number: 12,
                head_sha: HEAD.to_string(),
                dispatches_since: 1,
            }
        );
    }

    #[test]
    fn a_failed_remediation_dispatch_leaves_the_remediation_in_force() {
        // The spec re-dispatches until the attempt cap, not once. A dispatch
        // that did not fix it must not end the remediation.
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);

        succeeded(&db, 3, Some(HEAD));

        assert!(
            active_remediation(&db, &issue()).unwrap().is_some(),
            "a failed remediation dispatch leaves best_commit_sha untouched, \
so the remediation is still in force"
        );
    }

    #[test]
    fn a_newer_success_ends_the_remediation() {
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);
        assert!(active_remediation(&db, &issue()).unwrap().is_some());

        // The IC pushed the fix and the gate went green on the new commit.
        succeeded(&db, 3, Some(OTHER_HEAD));

        assert_eq!(
            active_remediation(&db, &issue()).unwrap(),
            None,
            "a success at a newer commit is the fix landing: the remediation \
is done and rung 10 reviews the new head"
        );
        assert!(
            get_remediation(&db, &issue()).unwrap().is_some(),
            "the record itself is kept — only its force is derived"
        );
    }

    #[test]
    fn the_superseded_test_is_case_insensitive() {
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);

        succeeded(&db, 2, Some(&HEAD.to_uppercase()));

        assert!(
            active_remediation(&db, &issue()).unwrap().is_some(),
            "one commit spelled two ways is one commit; reading it as two \
would re-dispatch against findings that were already fixed"
        );
    }

    #[test]
    fn an_unknown_commit_keeps_the_remediation_in_force() {
        let db = db();
        succeeded(&db, 2, None);
        arm(&db, HEAD, Some("f"), 5);
        assert_eq!(
            active_remediation(&db, &issue())
                .unwrap()
                .unwrap()
                .armed_at_commit,
            None
        );
        assert!(
            active_remediation(&db, &issue()).unwrap().is_some(),
            "unknown must not read as 'moved', or remediation is inoperative \
for any issue whose success recorded no commit"
        );
    }

    #[test]
    fn the_cap_is_checked_before_the_already_armed_test() {
        // The load-bearing ordering. An issue reaches its cap *while* a
        // remediation is in force — that is how an unproductive one ends —
        // and if `AlreadyArmed` shadowed the cap the caller would rehearse
        // the merge forever and rung 6's hold would never tell a human.
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);

        succeeded(&db, 5, Some(HEAD));

        let outcome = arm(&db, HEAD, Some("f"), 5);
        assert_eq!(
            outcome,
            ArmOutcome::CapReached {
                cumulative_attempt_n: 5,
                attempt_cap: 5,
            }
        );
        assert!(!outcome.in_force());
    }

    #[test]
    fn an_exhausted_issue_is_never_armed_in_the_first_place() {
        let db = db();
        succeeded(&db, 5, Some(HEAD));
        assert_eq!(
            arm(&db, HEAD, Some("f"), 5),
            ArmOutcome::CapReached {
                cumulative_attempt_n: 5,
                attempt_cap: 5,
            }
        );
        assert_eq!(
            get_remediation(&db, &issue()).unwrap(),
            None,
            "no record is written for an issue that can never be dispatched"
        );
    }

    #[test]
    fn a_pushed_fix_reviewed_again_arms_a_fresh_remediation() {
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("first round"), 5);
        succeeded(&db, 3, Some(OTHER_HEAD));

        let outcome = arm(&db, OTHER_HEAD, Some("second round"), 5);
        assert_eq!(
            outcome,
            ArmOutcome::Armed {
                pr_number: 12,
                head_sha: OTHER_HEAD.to_string(),
                has_findings: true,
            }
        );
        let record = active_remediation(&db, &issue()).unwrap().unwrap();
        assert_eq!(record.findings.as_deref(), Some("second round"));
        assert_eq!(record.armed_after_attempts, 3);
        assert_eq!(record.armed_at_commit.as_deref(), Some(OTHER_HEAD));
    }

    #[test]
    fn a_different_pr_on_the_same_commit_arms_separately() {
        // The key is (pr, commit). A branch reused for a second PR is a
        // different review of a different thing to merge.
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("pr 12"), 5);

        assert!(matches!(
            arm_pr(&db, 13, HEAD, Some("pr 13"), 5),
            ArmOutcome::Armed { pr_number: 13, .. }
        ));
        assert_eq!(
            active_remediation(&db, &issue())
                .unwrap()
                .unwrap()
                .pr_number,
            13
        );
    }

    #[test]
    fn findings_are_scrubbed_and_bounded_on_the_write_path() {
        let db = db();
        succeeded(&db, 1, Some(HEAD));
        let long = "x".repeat(MAX_FINDINGS_CHARS + 500);
        arm(&db, HEAD, Some(&long), 5);

        let stored = active_remediation(&db, &issue())
            .unwrap()
            .unwrap()
            .findings
            .unwrap();
        assert_eq!(
            stored.chars().count(),
            MAX_FINDINGS_CHARS,
            "findings are rendered into every remediation turn prompt, so an \
unbounded reason grows each dispatch's input cost without limit"
        );
    }

    #[test]
    fn clearing_removes_the_record_and_is_idempotent() {
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);

        assert!(clear_remediation(&db, &issue()).unwrap());
        assert_eq!(get_remediation(&db, &issue()).unwrap(), None);
        assert_eq!(active_remediation(&db, &issue()).unwrap(), None);
        assert!(!clear_remediation(&db, &issue()).unwrap());
    }

    #[test]
    fn a_cleared_issue_can_be_armed_again_by_a_later_review() {
        // Why clearing is a delete and not a flag: a human who clears one
        // remediation has not opted the issue out of every future one.
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);
        clear_remediation(&db, &issue()).unwrap();

        assert!(matches!(
            arm(&db, HEAD, Some("f"), 5),
            ArmOutcome::Armed { .. }
        ));
    }

    #[test]
    fn clearing_writes_a_wal_log_entry() {
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);
        clear_remediation(&db, &issue()).unwrap();

        let count: i64 = db
            .with_connection(|conn| {
                Ok(conn.query_row(
                    "SELECT COUNT(*) FROM wal_log WHERE operation = ?1",
                    rusqlite::params!["autopilot_clear_remediation"],
                    |row| row.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn remediation_records_do_not_collide_across_issues() {
        let db = db();
        let other = IssueRef::new(REPO, 999);
        succeeded(&db, 1, Some(HEAD));
        upsert_issue_status(
            &db,
            &IssueStatus {
                issue: other.clone(),
                best_verdict: Some(AttemptOutcome::Success),
                best_commit_sha: Some(OTHER_HEAD.to_string()),
                cumulative_attempt_n: 1,
            },
        )
        .unwrap();

        arm(&db, HEAD, Some("mine"), 5);
        assert_eq!(
            active_remediation(&db, &other).unwrap(),
            None,
            "one issue's remediation must not be readable as another's"
        );
    }

    #[test]
    fn an_invalid_repo_is_refused_before_anything_is_written() {
        let db = db();
        let bad = IssueRef::new("owner/repo\u{0}", 1);
        let err = arm_remediation(
            &db,
            &ArmRequest {
                issue: &bad,
                pr_number: 1,
                head_sha: HEAD,
                findings: None,
                attempt_cap: 5,
            },
        );
        assert!(err.is_err());
    }
}
