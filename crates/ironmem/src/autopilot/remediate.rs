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
//! *before* anything else — a remediation is charged against a cap of its
//! own, shared by every round, see [`attempts_charged`]), reviews are bounded
//! by rung 10's head-SHA trigger (no new commit, no new review, so a stuck
//! remediation is never re-billed for a second opinion on the same diff), and
//! the day is bounded by rung 5's ledger. Nothing here adds a fourth kind of
//! spend; it adds a new *reason* to spend on the two that exist.
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
//! **first**, ahead of the already-armed test, so a remediation that has spent
//! its own cap stops being carried by this module even though its remediation
//! record still exists — and rung 6's hold then runs for real, comments, and
//! flips the issue to `agent:blocked`. That is the spec's *"on exhaustion the
//! PR stays open for a human"*, and the ordering is what makes it reachable:
//! the other way round, an armed record would shadow the cap forever and the
//! human would never be told.
//!
//! # Its own budget, not the remainder ⟨#344⟩
//!
//! The spec's *"counting against the same per-issue attempt cap"* was first
//! built as one counter spanning the original work and every remediation
//! armed against it. The first live run showed what that means: an issue that
//! took two of five attempts to go green left its remediation three, and one
//! that went green on attempt five of five left it none — the harder an issue
//! was to get right, the less budget it had to answer its review. The cap is
//! still the same *value* (the caller passes the Lead's own `attempt_cap`),
//! but once an issue has succeeded it is charged only for the attempts since
//! its **first** success — every round of remediation shares that one
//! allowance, so the loop still halts. The baseline is derived from the
//! lineage rather than stored here, so neither a re-arm for a new commit nor
//! `--clear` can reset it; [`attempts_charged`] says why, and every place that
//! enforces the cap reads it.

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
/// `armed_after_attempts`. It does **not** reset the remediation's budget,
/// which is counted from the issue's first success in the lineage
/// ([`attempts_charged`]) — only `autopilot retry` restores that. Clearing is
/// therefore a "stop for now", and the
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
/// untouched by the no-downgrade rule, so the next tick tries again, against
/// the remediation's own cap — see [`attempts_charged`]), and not the passage
/// of time. The cap is what ends an unproductive one, and the
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

/// Attempts charged against the per-issue cap for an issue's next dispatch.
///
/// The single definition of the cap's operand, read by every place that
/// enforces it — [`super::run::run_issue`], [`super::queue::plan_queue`] and
/// [`arm_remediation`] — so a remediation is never armed under one count and
/// dispatched under another (it would either never fire or never stop).
///
/// # A remediation's own budget (#344)
///
/// The counter used to span the original work and every remediation armed
/// against it, so the harder an issue was to get right, the less budget it
/// had to answer its review: #339 went green on attempt 2 of 5 and its
/// remediation exhausted the 3 that were left, and an issue that went green
/// on its last attempt was never armed at all. So once an issue has
/// succeeded, the count starts at the attempt on which it **first**
/// succeeded: everything before that was the original work, and — because a
/// succeeded issue is only ever dispatched again by a remediation —
/// everything after it is remediation.
///
/// That baseline is *derived* from the append-only lineage, never stored on
/// the [`RemediationRecord`], and that is what keeps the loop bounded. A
/// pushed fix reviewed again arms a fresh record for a new commit, and
/// `--clear` deletes the record outright; a baseline kept on the record would
/// reset on both, and every new head would buy another full cap. Derived from
/// the first success, every round of review shares the one allowance, which
/// is the amendment's *"for any sequence of reviewer verdicts, the
/// remediation loop halts within its own cap"*.
///
/// A human retry still wins when it is later: the baseline is the later of
/// the two marks, so `autopilot retry` restores a full cap to an exhausted
/// remediation, and a remediation is never charged for attempts a retry
/// already forgave.
///
/// `succeeded` must be whether the issue's **status** records a success —
/// every caller derives it from `best_verdict`. It is not merely an
/// optimisation: the lineage and status writes are not one transaction, so a
/// crash between them can leave a success in the lineage that the status
/// never recorded, and passing `true` for that issue would credit attempts
/// it never earned. Given an honest `succeeded`, it fails closed: an issue
/// with no recorded successful attempt (or one numbered past the counter,
/// which only a hand-edited drawer could produce) gets no credit and is
/// charged exactly as before #344.
pub fn attempts_charged(
    db: &Database,
    issue: &IssueRef,
    cumulative_attempt_n: u32,
    succeeded: bool,
) -> Result<u32, MemoryError> {
    let forgiven = super::retry::forgiven_through(db, issue)?;
    let original_work = if succeeded {
        first_success_attempt(db, issue)?
            .filter(|&n| n <= cumulative_attempt_n)
            .unwrap_or(0)
    } else {
        0
    };
    Ok(super::retry::attempts_charged(
        cumulative_attempt_n,
        forgiven.max(original_work),
    ))
}

/// The attempt number on which the issue first recorded a success, if any.
///
/// Records are filtered to this issue, as [`super::retry::read_grant`] filters
/// its grant: two repos whose names collide on one knowledge-graph entity
/// would otherwise charge one issue's remediation for the other's attempts.
fn first_success_attempt(db: &Database, issue: &IssueRef) -> Result<Option<u32>, MemoryError> {
    Ok(lineage::attempts_for_issue(db, issue)?
        .into_iter()
        .filter(|attempt| attempt.issue == *issue && attempt.verdict == AttemptOutcome::Success)
        .map(|attempt| attempt.attempt_n)
        .min())
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
    /// The remediation has spent its own cap — every round of it, since the
    /// issue first succeeded. **Nothing is written, deliberately** (an
    /// existing record is left as it is, and keeps the Lead at the cap): this
    /// is the spec's *"on exhaustion the PR stays open for a human"*, and
    /// the caller's next step is rung 6's real hold, which comments and flips
    /// the issue to `agent:blocked` so a human is actually told.
    CapReached {
        cumulative_attempt_n: u32,
        attempt_cap: u32,
        /// Attempts charged to the remediation — dispatched since the issue
        /// first succeeded (or since a later human retry; the lifetime count
        /// when the lineage records no success). This, not the lifetime
        /// `cumulative_attempt_n`, is what reached `attempt_cap`.
        attempts_charged: u32,
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
/// is load-bearing rather than incidental. A remediation reaches its own cap
/// while it is still in force — that is the ordinary way an unproductive one
/// ends — and testing "already armed" first would return
/// `AlreadyArmed` forever, so the caller would rehearse the merge on every
/// pass and rung 6's hold would never run for real. The PR would then sit
/// open, unmerged and un-commented, with nobody told: *"never merged with an
/// unresolved finding"* satisfied by accident and *"the PR stays open for a
/// human"* silently unsatisfied.
///
/// Pure enough to test exhaustively: it reads the issue's status, lineage and
/// retry grant and its own record, and writes at most one drawer.
pub fn arm_remediation(db: &Database, request: &ArmRequest) -> Result<ArmOutcome, MemoryError> {
    validate_repo(&request.issue.repo)?;
    let status = lineage::get_issue_status(db, request.issue)?;
    let cumulative_attempt_n = status
        .as_ref()
        .map(|s| s.cumulative_attempt_n)
        .unwrap_or_default();

    // The remediation's own budget (#344), read through the same
    // [`attempts_charged`] the Lead enforces, on every path — including a
    // fresh arm for a new commit. A pushed fix reviewed again is a new round
    // of the same remediation, not a new budget; charging a fresh record at
    // zero would let every new head buy another full cap.
    //
    // Charged, not lifetime: a human retry forgives attempts against the cap
    // wherever the cap is read, and reading the raw counter here would keep
    // reporting `CapReached` for an issue whose budget was just restored.
    // `armed_after_attempts` below stays the lifetime number, so "dispatches
    // since armed" keeps measuring dispatches rather than sliding backwards
    // the moment a grant is written.
    let succeeded = status
        .as_ref()
        .is_some_and(|s| s.best_verdict == Some(AttemptOutcome::Success));
    let charged = attempts_charged(db, request.issue, cumulative_attempt_n, succeeded)?;
    if charged >= request.attempt_cap {
        return Ok(ArmOutcome::CapReached {
            cumulative_attempt_n,
            attempt_cap: request.attempt_cap,
            attempts_charged: charged,
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
    use crate::autopilot::lineage::{
        record_attempt, upsert_issue_status, AttemptRecord, IssueStatus,
    };
    use crate::autopilot::retry;

    const REPO: &str = "ironrace/ironmem";
    const HEAD: &str = "a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2";
    const OTHER_HEAD: &str = "ffffffffffffffffffffffffffffffffffffffff";

    fn issue() -> IssueRef {
        IssueRef::new(REPO, 283)
    }

    fn db() -> Database {
        Database::open_in_memory().unwrap()
    }

    /// A succeeded issue at `attempts`, green at `commit`: a successful
    /// attempt numbered `attempts` is appended to the lineage, as `run_issue`
    /// records one. Only the *first* such attempt sets the remediation
    /// budget's baseline, so calling this again models a remediation round
    /// that went green.
    fn succeeded(db: &Database, attempts: u32, commit: Option<&str>) {
        record_attempt(
            db,
            &AttemptRecord {
                issue: issue(),
                attempt_n: attempts,
                approach: "went green".to_string(),
                verdict: AttemptOutcome::Success,
                why_failed: None,
                commit_sha: commit.map(str::to_string),
            },
        )
        .unwrap();
        spent(db, attempts, commit);
    }

    /// Move the counter to `attempts` without recording a success — failed
    /// dispatches since. `commit` stays the issue's best (the no-downgrade
    /// rule), so a remediation armed against it stays in force.
    fn spent(db: &Database, attempts: u32, commit: Option<&str>) {
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
        spent(&db, 3, Some(HEAD));

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

        spent(&db, 3, Some(HEAD));

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

        spent(&db, 2, Some(&HEAD.to_uppercase()));

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

        // Five failed remediation dispatches since the success at attempt 2.
        spent(&db, 7, Some(HEAD));

        let outcome = arm(&db, HEAD, Some("f"), 5);
        assert_eq!(
            outcome,
            ArmOutcome::CapReached {
                cumulative_attempt_n: 7,
                attempt_cap: 5,
                attempts_charged: 5,
            }
        );
        assert!(!outcome.in_force());
    }

    #[test]
    fn a_remediation_budget_is_counted_from_the_first_success() {
        // Issue #344. The attempts the original work spent are not the
        // remediation's to lose: green at attempt 4 of a cap of 5, the
        // remediation still has five dispatches, not one.
        let db = db();
        succeeded(&db, 4, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);

        spent(&db, 8, Some(HEAD));
        assert_eq!(
            arm(&db, HEAD, Some("f"), 5),
            ArmOutcome::AlreadyArmed {
                pr_number: 12,
                head_sha: HEAD.to_string(),
                dispatches_since: 4,
            },
            "four remediation dispatches of five: still in force"
        );

        spent(&db, 9, Some(HEAD));
        assert!(
            !arm(&db, HEAD, Some("f"), 5).in_force(),
            "the fifth spends the remediation's own cap"
        );
    }

    #[test]
    fn a_success_on_the_last_attempt_can_still_be_remediated() {
        // Issue #344's boundary: success at attempt N of N used to leave a
        // remediation no budget at all — `arm_remediation` answered
        // `CapReached`, so the first review asking for changes went straight
        // to the human hold.
        let db = db();
        succeeded(&db, 5, Some(HEAD));
        assert_eq!(
            arm(&db, HEAD, Some("f"), 5),
            ArmOutcome::Armed {
                pr_number: 12,
                head_sha: HEAD.to_string(),
                has_findings: true,
            }
        );
        assert_eq!(
            get_remediation(&db, &issue())
                .unwrap()
                .unwrap()
                .armed_after_attempts,
            5
        );
    }

    #[test]
    fn a_new_head_reviewed_again_does_not_buy_a_fresh_cap() {
        // The round is new; the budget is not. A remediation dispatch that
        // pushes a commit and fails the gate still moves the PR head, rung 10
        // reviews the new head, and a second `needs_changes` arms a record
        // for a different commit. Charging that record from zero would hand
        // every pushed commit another full cap and the loop would never end.
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        assert!(arm(&db, HEAD, Some("round 1"), 5).in_force());

        spent(&db, 6, Some(HEAD));
        assert!(
            arm(&db, OTHER_HEAD, Some("round 2"), 5).in_force(),
            "four of five spent: the new round is armed"
        );

        spent(&db, 7, Some(HEAD));
        assert_eq!(
            arm(
                &db,
                "0123456789abcdef0123456789abcdef01234567",
                Some("round 3"),
                5
            ),
            ArmOutcome::CapReached {
                cumulative_attempt_n: 7,
                attempt_cap: 5,
                attempts_charged: 5,
            },
            "a third head is still charged from the first success"
        );
        assert_eq!(
            get_remediation(&db, &issue()).unwrap().unwrap().head_sha,
            OTHER_HEAD,
            "a refused arm writes nothing: the record in force is untouched"
        );
    }

    #[test]
    fn the_remediation_loop_halts_within_its_own_cap_for_any_sequence_of_rounds() {
        // The amendment's termination requirement: "for any sequence of
        // reviewer verdicts, the remediation loop halts within its own cap".
        // Each round is a `needs_changes` — about a new head or the same one
        // again — followed by a run that spends one to three attempts
        // (bounded, like `run_issue`, by the same `attempts_charged`), and
        // which may or may not go green. Whatever the sequence, exactly one
        // cap of remediation attempts is spent, and the loop ends on
        // `CapReached`, which is what sends the PR to the human hold.
        for cap in 1..=5u32 {
            for first_success in 1..=cap {
                for pattern in 0..8u32 {
                    let rounds_go_green = pattern & 1 == 1;
                    let reuse_heads = pattern & 2 == 2;
                    let multi_attempt = pattern & 4 == 4;
                    let db = db();
                    succeeded(&db, first_success, Some("c0"));
                    let mut attempts = first_success;
                    let mut spent_since = 0u32;
                    let mut ended_on_cap = false;
                    for round in 1..=100u32 {
                        let head = if reuse_heads {
                            format!("{:040x}", round / 2)
                        } else {
                            format!("{round:040x}")
                        };
                        match arm(&db, &head, Some("again"), cap) {
                            ArmOutcome::CapReached { .. } => {
                                ended_on_cap = true;
                                break;
                            }
                            outcome => assert!(outcome.in_force(), "{outcome:?}"),
                        }
                        let wanted = if multi_attempt { 1 + round % 3 } else { 1 };
                        for _ in 0..wanted {
                            if attempts_charged(&db, &issue(), attempts, true).unwrap() >= cap {
                                break;
                            }
                            attempts += 1;
                            spent_since += 1;
                            spent(&db, attempts, Some("c0"));
                        }
                        if rounds_go_green {
                            succeeded(&db, attempts, Some(&format!("c{round}")));
                        }
                    }
                    assert!(
                        ended_on_cap,
                        "cap {cap}, first success {first_success}, pattern {pattern}: \
the loop must end on CapReached"
                    );
                    assert_eq!(
                        spent_since, cap,
                        "cap {cap}, first success {first_success}, pattern {pattern}: \
exactly one cap of remediation attempts"
                    );
                }
            }
        }
    }

    #[test]
    fn clearing_a_remediation_does_not_refill_its_budget() {
        // `--clear` deletes the record, and the next advance pass re-arms the
        // same commit. That is a "stop for now", not `autopilot retry` under
        // another name: the budget is counted from the lineage, which a clear
        // does not touch.
        let db = db();
        succeeded(&db, 2, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);
        spent(&db, 7, Some(HEAD));
        assert!(!arm(&db, HEAD, Some("f"), 5).in_force());

        assert!(clear_remediation(&db, &issue()).unwrap());
        assert!(
            !arm(&db, HEAD, Some("f"), 5).in_force(),
            "a cleared remediation re-armed is still exhausted"
        );
        assert_eq!(
            get_remediation(&db, &issue()).unwrap(),
            None,
            "and nothing is written: a record here would put the issue back in \
front of the Lead"
        );
    }

    #[test]
    fn a_human_retry_restores_an_exhausted_remediation() {
        // The later mark wins: a retry granted after the remediation spent
        // its cap gives it a full one back. The queue and the run loop read
        // the same `attempts_charged`, so they agree.
        let db = db();
        succeeded(&db, 5, Some(HEAD));
        arm(&db, HEAD, Some("f"), 5);
        spent(&db, 10, Some(HEAD));
        assert!(!arm(&db, HEAD, Some("f"), 5).in_force());

        retry::grant_retry(&db, &issue(), 10).unwrap();
        assert!(
            arm(&db, HEAD, Some("f"), 5).in_force(),
            "a forgiven remediation is in force again"
        );
    }

    #[test]
    fn an_issue_with_no_recorded_success_gets_no_remediation_credit() {
        // Fails closed. A status that claims a success the lineage does not
        // hold (a hand-edited drawer, or lineage removed from under it) is
        // charged exactly as before #344 rather than handed a budget.
        let db = db();
        spent(&db, 5, Some(HEAD));
        assert!(!arm(&db, HEAD, Some("f"), 5).in_force());
    }

    #[test]
    fn attempts_charged_counts_from_the_later_of_the_first_success_and_a_retry() {
        let db = db();
        // No success recorded: lifetime count, less any retry.
        assert_eq!(attempts_charged(&db, &issue(), 5, true).unwrap(), 5);

        succeeded(&db, 3, Some(HEAD));
        succeeded(&db, 4, Some(OTHER_HEAD));
        assert_eq!(
            attempts_charged(&db, &issue(), 6, true).unwrap(),
            3,
            "from the FIRST success (3), not the latest (4)"
        );
        assert_eq!(
            attempts_charged(&db, &issue(), 6, false).unwrap(),
            6,
            "an issue that has not succeeded gets no remediation credit"
        );
        assert_eq!(
            attempts_charged(&db, &issue(), 2, true).unwrap(),
            2,
            "a success numbered past the counter is a corrupt read: no credit"
        );

        retry::grant_retry(&db, &issue(), 5).unwrap();
        assert_eq!(
            attempts_charged(&db, &issue(), 6, true).unwrap(),
            1,
            "a later retry wins"
        );
    }

    #[test]
    fn a_success_after_a_retry_is_still_the_remediation_baseline() {
        // #333's recovery followed by #344's: the original work exhausts its
        // cap, a human retries, and it goes green two attempts later. The
        // remediation is charged from that success, not from the retry —
        // otherwise the attempts after the retry are taken from it again.
        let db = db();
        spent(&db, 5, None);
        retry::grant_retry(&db, &issue(), 5).unwrap();
        succeeded(&db, 7, Some(HEAD));
        assert_eq!(attempts_charged(&db, &issue(), 9, true).unwrap(), 2);
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
