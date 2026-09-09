//! Human-authorized retry of an issue that hit its per-issue attempt cap.
//!
//! # The gap this closes
//!
//! The spec's label table is unambiguous about how an exhausted issue comes
//! back:
//!
//! > | `agent:exhausted` | Per-issue attempt cap hit | **Never self-resumes**;
//! > human must re-label |
//!
//! and its stagnation section repeats it: *"`agent:exhausted` never
//! self-resumes. Only a human re-labeling it retries."*
//!
//! That recovery has never worked. The label governs *selection* — the Lead
//! lists `agent:ready` issues, and [`super::labels::eligibility`] decides
//! which of them are dispatchable — but the cap is enforced somewhere else
//! entirely, against `cumulative_attempt_n` in the issue-status drawer, which
//! is cumulative across runs and which **nothing anywhere resets**. Not
//! [`super::blocked`], not [`super::labels`], not `autopilot exhaust`, and
//! there was no reset command. So a human who did exactly what the spec says
//! got the issue selected and then handed straight back as
//! [`super::run::TerminalReason::AttemptCapExhausted`] without a dispatch, on
//! every tick, forever. The only escape was raising `--attempt-cap`, which is
//! a global knob being used to express a per-issue intent — and which raises
//! the ceiling for every other issue at the same time.
//!
//! It is the same shape as the three defects the first live run found: a rule
//! that is true of the thing the operator can see (the label) and false of the
//! counter actually deciding the outcome.
//!
//! # Why forgiveness rather than a reset
//!
//! The obvious fix is to set `cumulative_attempt_n` back to zero. It is
//! wrong, for a reason that only shows up in the next dispatch's prompt.
//!
//! That counter is not only a budget, it is the **numbering** for the attempt
//! records: [`super::lineage::record_attempt`] stores it, and
//! [`super::turn_prompt`] renders each prior attempt as `attempt {n}: ...` so
//! a re-dispatched IC can read what has already been tried. Zeroing it makes
//! the next attempt `#1` again, so the IC is handed two different `attempt 1`
//! lines describing different work — degrading precisely the history that
//! section exists to convey, and doing so at the moment it matters most,
//! because an issue being retried is one with a lot of history.
//!
//! So the lifetime counter stays monotonic and this module records the
//! forgiveness beside it: a grant says *"attempts up to and including N are
//! spent, and do not count against the cap any more."* The cap is then read
//! as `cumulative_attempt_n - forgiven_through`, which is attempts **since
//! the last human retry** — exactly the quantity the spec's per-issue cap is
//! trying to bound. Both numbers survive, so "12 attempts over three human
//! retries" stays a readable fact rather than three indistinguishable runs of
//! four.
//!
//! # Why a grant is not the same as clearing a stop
//!
//! A retry grant deliberately touches **only** the attempt cap. It is not a
//! general "unstick this issue" verb, because the other stop states are not
//! the same kind of thing and must not be cleared by a command whose name
//! says nothing about them:
//!
//! - an escalation ([`super::supervise`]) says a *strategy* is failing, and is
//!   cleared on its own terms;
//! - `agent:blocked` says a question is outstanding, and
//!   [`super::blocked`]'s answer path is what retires it;
//! - a recorded success is never downgraded, on the same principle
//!   [`super::remediate`] follows.
//!
//! Granting a retry against an issue in one of those states is legal and
//! simply insufficient: the run stops for the other reason, which is the
//! honest outcome and is reported as such rather than silently widened.

use serde::{Deserialize, Serialize};

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::{read_current, validate_repo, write_current, IssueRef};

/// A human's authorization to keep working an issue that hit its cap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetryGrant {
    pub issue: String,
    pub repo: String,
    pub issue_number: u64,
    /// Attempts up to and including this number no longer count against the
    /// per-issue cap. Compared against `cumulative_attempt_n`, never against
    /// any single dispatch's own `attempt_n`.
    pub forgiven_through: u32,
    pub granted_at: String,
}

fn retry_key(issue: &IssueRef) -> String {
    format!("retry-grant:{}", issue.slug())
}

/// How many of this issue's attempts have been forgiven, or `0` if none.
///
/// `0` and "no grant" are deliberately the same answer: a grant forgiving
/// nothing bounds nothing, so there is no state a caller could act on
/// differently.
pub fn forgiven_through(db: &Database, issue: &IssueRef) -> Result<u32, MemoryError> {
    Ok(read_grant(db, issue)?
        .map(|grant| grant.forgiven_through)
        .unwrap_or(0))
}

/// Read an issue's retry grant, if one has been made.
pub fn read_grant(db: &Database, issue: &IssueRef) -> Result<Option<RetryGrant>, MemoryError> {
    validate_repo(&issue.repo)?;
    let Some(drawer) = read_current(db, &retry_key(issue))? else {
        return Ok(None);
    };
    let grant: RetryGrant = serde_json::from_str(&drawer.content)?;
    // A grant filed under this issue's key that names a different issue is a
    // slug collision, not this issue's grant. Forgiving attempts on the
    // strength of another issue's record would raise a cap nobody raised.
    if grant.repo != issue.repo || grant.issue_number != issue.number {
        return Ok(None);
    }
    Ok(Some(grant))
}

/// Remaining attempts against `cap`, given what has been forgiven.
///
/// Saturating on both sides: a grant forgiving more than has been attempted
/// (a cap lowered between runs, or a hand-edited drawer) leaves the full cap
/// rather than wrapping into a huge budget.
pub fn attempts_charged(cumulative_attempt_n: u32, forgiven_through: u32) -> u32 {
    cumulative_attempt_n.saturating_sub(forgiven_through)
}

/// What a retry request did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum GrantOutcome {
    /// A grant was written; the issue has its full cap again.
    Granted { forgiven_through: u32 },
    /// The issue has never recorded an attempt, so there is nothing to
    /// forgive. Not an error: the caller's intent ("let this issue run") is
    /// already true, and writing a grant forgiving zero attempts would record
    /// a decision nobody made.
    NothingToForgive,
    /// Every attempt this issue has made is already forgiven — a repeated
    /// retry with no dispatch in between. Idempotent rather than additive:
    /// two retries must not buy two caps' worth of budget.
    AlreadyForgiven { forgiven_through: u32 },
}

/// Forgive every attempt this issue has made so far.
///
/// Takes the count rather than reading it, so the caller decides which status
/// read the decision is made against, and a stale one cannot silently forgive
/// attempts made since.
pub fn grant_retry(
    db: &Database,
    issue: &IssueRef,
    cumulative_attempt_n: u32,
) -> Result<GrantOutcome, MemoryError> {
    validate_repo(&issue.repo)?;
    if cumulative_attempt_n == 0 {
        return Ok(GrantOutcome::NothingToForgive);
    }
    // Idempotence is checked before the write, not after: a second retry
    // issued against the same unchanged count must be a no-op, or an operator
    // repeating a command they were unsure had landed doubles the budget.
    let existing = forgiven_through(db, issue)?;
    if existing >= cumulative_attempt_n {
        return Ok(GrantOutcome::AlreadyForgiven {
            forgiven_through: existing,
        });
    }
    let grant = RetryGrant {
        issue: issue.canonical(),
        repo: issue.repo.clone(),
        issue_number: issue.number,
        forgiven_through: cumulative_attempt_n,
        granted_at: chrono::Utc::now().to_rfc3339(),
    };
    write_current(db, &retry_key(issue), &serde_json::to_string(&grant)?)?;
    Ok(GrantOutcome::Granted {
        forgiven_through: cumulative_attempt_n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::lineage::{self, IssueStatus};
    use crate::autopilot::AttemptOutcome;

    fn issue() -> IssueRef {
        IssueRef::new("ironrace/ironmem", 285)
    }

    fn db_with_attempts(n: u32) -> Database {
        let db = Database::open_in_memory().unwrap();
        if n > 0 {
            lineage::upsert_issue_status(
                &db,
                &IssueStatus {
                    issue: issue(),
                    best_verdict: Some(AttemptOutcome::Failed),
                    best_commit_sha: None,
                    cumulative_attempt_n: n,
                },
            )
            .unwrap();
        }
        db
    }

    #[test]
    fn no_grant_forgives_nothing() {
        let db = db_with_attempts(0);
        assert_eq!(forgiven_through(&db, &issue()).unwrap(), 0);
        assert!(read_grant(&db, &issue()).unwrap().is_none());
    }

    #[test]
    fn a_grant_forgives_every_attempt_made_so_far() {
        let db = db_with_attempts(5);
        assert_eq!(
            grant_retry(&db, &issue(), 5).unwrap(),
            GrantOutcome::Granted {
                forgiven_through: 5
            }
        );
        assert_eq!(forgiven_through(&db, &issue()).unwrap(), 5);
        // The whole point: the cap now sees nothing charged against it, while
        // the lifetime count is untouched.
        assert_eq!(attempts_charged(5, 5), 0);
        let status = lineage::get_issue_status(&db, &issue()).unwrap().unwrap();
        assert_eq!(
            status.cumulative_attempt_n, 5,
            "the lifetime counter must stay monotonic so attempt numbering does not collide"
        );
    }

    #[test]
    fn a_repeated_retry_does_not_buy_a_second_budget() {
        let db = db_with_attempts(5);
        grant_retry(&db, &issue(), 5).unwrap();
        assert_eq!(
            grant_retry(&db, &issue(), 5).unwrap(),
            GrantOutcome::AlreadyForgiven {
                forgiven_through: 5
            }
        );
        assert_eq!(forgiven_through(&db, &issue()).unwrap(), 5);
    }

    #[test]
    fn a_later_retry_forgives_the_attempts_made_since_the_last_one() {
        let db = db_with_attempts(5);
        grant_retry(&db, &issue(), 5).unwrap();
        // Five more dispatches later.
        assert_eq!(
            grant_retry(&db, &issue(), 10).unwrap(),
            GrantOutcome::Granted {
                forgiven_through: 10
            }
        );
        assert_eq!(attempts_charged(10, 10), 0);
    }

    #[test]
    fn an_issue_that_has_never_attempted_anything_records_no_grant() {
        let db = db_with_attempts(0);
        assert_eq!(
            grant_retry(&db, &issue(), 0).unwrap(),
            GrantOutcome::NothingToForgive
        );
        assert!(
            read_grant(&db, &issue()).unwrap().is_none(),
            "a grant forgiving nothing records a decision nobody made"
        );
    }

    #[test]
    fn attempts_charged_saturates_rather_than_wrapping() {
        // A cap lowered between runs, or a hand-edited drawer: forgiving more
        // than was attempted must leave zero charged, never u32::MAX.
        assert_eq!(attempts_charged(3, 9), 0);
        assert_eq!(attempts_charged(9, 3), 6);
    }

    #[test]
    fn a_slug_colliding_grant_is_not_read_as_this_issues() {
        let db = Database::open_in_memory().unwrap();
        let other = IssueRef::new("ironrace/ironmem", 999);
        grant_retry(&db, &other, 4).unwrap();
        // Same repo, different number: the guard is on the record's contents,
        // not on the key, because the key is a slug and slugs can collide.
        let stolen = RetryGrant {
            issue: other.canonical(),
            repo: other.repo.clone(),
            issue_number: other.number,
            forgiven_through: 4,
            granted_at: chrono::Utc::now().to_rfc3339(),
        };
        write_current(
            &db,
            &retry_key(&issue()),
            &serde_json::to_string(&stolen).unwrap(),
        )
        .unwrap();
        assert!(read_grant(&db, &issue()).unwrap().is_none());
        assert_eq!(forgiven_through(&db, &issue()).unwrap(), 0);
    }
}
