//! Supervision and crash safety — build-ladder rung 7.
//!
//! Three things the spec requires and no earlier rung built:
//!
//! 1. **process-health** — "is the IC alive and making progress?"
//!    ([`assess_process_health`]).
//! 2. **strategy-health** — "is it alive but thrashing the same failure?"
//!    ([`assess_strategy_health`]).
//! 3. **Dispatch-state reconciliation on Lead restart** — the spec's
//!    *Lead crash-safe state* table ([`reconcile`]).
//!
//! The spec is explicit that 1 and 2 are **not** redundant: "An IC can be
//! perfectly healthy and still completely stuck."
//!
//! # The false-positive guard is the whole design of process-health
//!
//! The naive check — "not in the registry, therefore dead" — is wrong twice
//! over here. An IC mid-long-turn legitimately cannot answer, and rung 4
//! measured that `-p` sessions expose no `status` field at all, so there is no
//! busy/idle signal to fall back on. The spec's rule is therefore an **AND
//! over two different clocks**:
//!
//! > Declare it dead only when *both* a liveness ping goes unanswered past a
//! > short timeout **and** its checkpoint/lineage state has not advanced
//! > within a longer window.
//!
//! [`assess_process_health`] implements exactly that and nothing looser.
//! `absent_for` and `stalled_for` are separate measurements against separate
//! thresholds, and [`ProcessHealth::SilentNotDead`] records *which* of the two
//! spared the IC, so a false positive that nearly happened is visible rather
//! than inferred.
//!
//! A third state exists above both: [`ProcessHealth::Unknown`]. The registry
//! can fail to answer, and "could not ask" is not "not listed" — see
//! [`super::registry`]'s module doc for why that distinction is load-bearing.
//!
//! # What "progress" can actually mean here
//!
//! The spec says "checkpoint/lineage state". The IC is instructed to
//! checkpoint every turn, but that checkpoint lands in its *transcript*, which
//! `--resume` carries and no external supervisor can read. What a supervisor
//! written in Rust can observe is the persisted state: the dispatch-state
//! drawer's transitions and the issue's lineage. [`progress_fingerprint`]
//! composes those into one string, and **any change to it is progress**.
//!
//! This is deliberately a *coarse* signal, and coarse in the safe direction:
//! it can miss progress (an IC working hard inside a single long dispatch
//! moves nothing observable), never invent it. Missing progress alone cannot
//! kill an IC, because the liveness clock has to agree — which is the second
//! reason the spec's AND is not optional.
//!
//! # Rung 7's own supervision state, and why it needs a drawer
//!
//! Both clocks measure *elapsed time since a transition*, and a transition is
//! not visible in a point-in-time read: "the session is absent right now" and
//! "the session has been absent for six minutes" are different facts, and only
//! the second is what the spec asks about. So supervision keeps its own
//! record — an eighth drawer kind, of the module doc's kind-2 shape,
//! `logical_key` per issue — remembering when the fingerprint last moved and
//! when the session was first seen missing. Without it every check would be
//! the first check, and no IC could ever be declared dead.
//!
//! # Redirect, then escalate — and never more than once each
//!
//! The spec's strategy-health action is "redirect strategy, or stop and
//! escalate", without saying which applies when. [`plan_supervision`] makes it
//! deterministic: the *first* time a failure signature thrashes, issue a
//! redirect (delivered through rung 2's already-provisioned
//! [`super::turn_prompt::TurnPromptInputs::strategy_redirect`] slot); if the
//! **same** signature thrashes again after that redirect was in force, the
//! redirect did not work, and it escalates. Never a third round of the same
//! thing — rung 6's lesson 19: anything a poll loop calls has to be
//! idempotent, per action, by design. A redirect is issued once per signature
//! and an escalation is recorded once per signature, so a supervisor run on a
//! cron does not re-issue either.
//!
//! # A note for OQ9
//!
//! The spec's open question 9 asks whether the Lead needs to be a Claude
//! session at all. Everything in this module is mechanical — two clocks, a
//! string comparison, and a table lookup — which is further evidence for the
//! Rust-supervisor recommendation. The one genuinely model-shaped judgment,
//! *what* a redirect should say, is the exception: [`redirect_text`] can only
//! name the repeated failure and forbid repeating it. A Lead with cross-repo
//! context could say something better. That limit is stated here rather than
//! hidden, because it is the actual boundary between the two answers.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::dispatch_state::{self, DispatchState};
use super::lineage::{self, AttemptOutcome, AttemptRecord, IssueStatus};
use super::registry::{Liveness, RegistrySnapshot};
use super::scrub::scrub_and_bound;
use super::{read_current, validate_repo, write_current, IssueRef};

/// The short timeout: how long a session must be absent from the registry
/// before absence counts toward death.
///
/// Operator-tunable placeholder. The spec says "a short timeout" and names no
/// figure; two minutes comfortably covers a registry read racing a dispatch
/// boundary (an IC process legitimately exits at the end of every dispatch and
/// is absent until the next one starts) without being so long that a genuinely
/// dead IC sits unnoticed for an hour.
pub const DEFAULT_LIVENESS_GRACE_SECS: u64 = 120;

/// The longer window: how long the observable lineage/dispatch state must sit
/// unchanged before staleness counts toward death.
///
/// Operator-tunable placeholder, and the more consequential of the two — see
/// [`SupervisionConfig::validate`] on why it must exceed the grace. Fifteen
/// minutes is longer than any dispatch rung 0 or rung 2 measured (8–20
/// seconds) by two orders of magnitude, deliberately: those probes were
/// lightweight, a real gate suite is not, and the honest response to an
/// unmeasured duration is a generous bound rather than a confident one.
pub const DEFAULT_PROGRESS_WINDOW_SECS: u64 = 900;

/// How many consecutive failed attempts sharing one failure signature count
/// as thrashing.
///
/// The spec's Testing table asks for "feed identical failure repeatedly →
/// thrash detection fires" and names no count. Three is the smallest number
/// that is unambiguously a pattern rather than a coincidence: two identical
/// failures is a retry, which is the normal and intended behavior of the
/// attempt loop.
pub const DEFAULT_THRASH_THRESHOLD: u32 = 3;

/// Bound on a stored failure signature. Long enough to keep a real assertion
/// or test name distinguishing, short enough that a supervision record cannot
/// grow to the size of the lineage it summarizes.
pub const MAX_SIGNATURE_CHARS: usize = 500;

/// Bound on the generated redirect text, which is inlined into the next
/// dispatch's `/goal` condition — a budget the spec caps at 4,000 characters
/// in total, shared with the issue body and the whole lineage section.
pub const MAX_REDIRECT_CHARS: usize = 800;

/// Upper bound on in-flight dispatch-state drawers [`reconcile`] will read.
///
/// Applied to the dispatch-state rows *specifically* (see
/// [`dispatch_state::all_dispatch_states`]), never to the room as a whole.
/// Rung 5's finding #4 is the reason: this room also holds every append-only
/// attempt, review and merge record, so a limit applied across all of them
/// newest-first would let ordinary lineage traffic push the in-flight
/// dispatch states out of the window — and a reconciliation that cannot see a
/// dispatch state reports its live IC as an *orphan*, which is a wrong answer,
/// not an error.
pub const MAX_IN_FLIGHT_DISPATCH_STATES: usize = 10_000;

/// Thresholds for the two checks.
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisionConfig {
    pub liveness_grace_secs: u64,
    pub progress_window_secs: u64,
    pub thrash_threshold: u32,
}

impl Default for SupervisionConfig {
    fn default() -> Self {
        Self {
            liveness_grace_secs: DEFAULT_LIVENESS_GRACE_SECS,
            progress_window_secs: DEFAULT_PROGRESS_WINDOW_SECS,
            thrash_threshold: DEFAULT_THRASH_THRESHOLD,
        }
    }
}

impl SupervisionConfig {
    /// Reject a configuration whose two clocks cannot express the spec's rule.
    ///
    /// The grace must be strictly shorter than the progress window. If they
    /// were equal or inverted, the two conditions would reach their
    /// thresholds together and the AND would collapse into a single signal —
    /// which is precisely the ping-alone check the spec forbids, arrived at by
    /// configuration instead of by code.
    pub fn validate(&self) -> Result<(), MemoryError> {
        if self.liveness_grace_secs == 0 {
            return Err(MemoryError::Validation(
                "liveness_grace_secs must be at least 1 — a zero grace makes a single \
                 point-in-time registry read sufficient to declare an IC dead"
                    .into(),
            ));
        }
        if self.progress_window_secs <= self.liveness_grace_secs {
            return Err(MemoryError::Validation(format!(
                "progress_window_secs ({}) must exceed liveness_grace_secs ({}) — the spec's \
                 death rule is an AND over a short and a longer clock, and equal clocks \
                 collapse it into the ping-alone check it forbids",
                self.progress_window_secs, self.liveness_grace_secs
            )));
        }
        if self.thrash_threshold < 2 {
            return Err(MemoryError::Validation(
                "thrash_threshold must be at least 2 — a single failure is an attempt, not a \
                 pattern"
                    .into(),
            ));
        }
        Ok(())
    }
}

// ── the supervision record (drawer kind 8, kind-2 shape) ────────────────

/// Rung 7's own per-issue state: the two clocks' origins, plus which
/// strategy interventions have already been made.
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisionRecord {
    pub issue: IssueRef,
    /// The observable state as of the last check — see
    /// [`progress_fingerprint`].
    pub fingerprint: String,
    /// When `fingerprint` last *changed*. The origin of the progress clock.
    pub progress_observed_at: String,
    /// When the session was first observed missing from the registry, or
    /// `None` if it was listed (or unreadable) at the last check. The origin
    /// of the liveness clock.
    pub first_absent_at: Option<String>,
    pub last_checked_at: String,
    /// A strategy redirect in force for the next dispatch, consumed by
    /// [`super::run::run_issue`] through the turn prompt.
    pub active_redirect: Option<String>,
    /// The failure signature `active_redirect` was issued for. A *different*
    /// signature thrashing later is new information and earns its own
    /// redirect; the same one thrashing again means the redirect failed.
    pub redirect_signature: Option<String>,
    /// How many real attempts this issue had when `active_redirect` was
    /// issued.
    ///
    /// Without it, escalation is triggered by *being polled twice* rather
    /// than by the redirect failing. A supervisor on a cron would arm the
    /// redirect on one pass and escalate it on the next, over byte-identical
    /// lineage, before any dispatch ever read it — which is not what
    /// "the redirect did not work" means, and hands the IC's one steer back
    /// unused. Escalation now additionally requires that the attempt count
    /// has *moved* since, i.e. a dispatch actually consumed the redirect and
    /// still failed the same way.
    ///
    /// `#[serde(default)]` → records written before this field existed read
    /// back as `None`, which suppresses escalation until the next redirect
    /// is issued. That is the conservative direction: it delays a human
    /// notification rather than firing one on no evidence.
    pub redirect_issued_after_attempts: Option<u32>,
    /// The signature already escalated on, so a poll loop escalates once.
    pub escalated_signature: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SupervisionBody {
    issue: u64,
    repo: String,
    fingerprint: String,
    progress_observed_at: String,
    first_absent_at: Option<String>,
    last_checked_at: String,
    active_redirect: Option<String>,
    redirect_signature: Option<String>,
    #[serde(default)]
    redirect_issued_after_attempts: Option<u32>,
    escalated_signature: Option<String>,
}

fn supervision_key(issue: &IssueRef) -> String {
    format!("supervision:{}", issue.slug())
}

/// Write (overwrite) an issue's supervision record.
pub fn upsert_supervision(
    db: &Database,
    record: &SupervisionRecord,
) -> Result<String, MemoryError> {
    validate_repo(&record.issue.repo)?;
    let body = SupervisionBody {
        issue: record.issue.number,
        repo: record.issue.repo.clone(),
        fingerprint: record.fingerprint.clone(),
        progress_observed_at: record.progress_observed_at.clone(),
        first_absent_at: record.first_absent_at.clone(),
        last_checked_at: record.last_checked_at.clone(),
        active_redirect: record.active_redirect.clone(),
        redirect_signature: record.redirect_signature.clone(),
        redirect_issued_after_attempts: record.redirect_issued_after_attempts,
        escalated_signature: record.escalated_signature.clone(),
    };
    let content = serde_json::to_string(&body)?;
    write_current(db, &supervision_key(&record.issue), &content)
}

/// Read an issue's supervision record, if it has ever been checked.
pub fn get_supervision(
    db: &Database,
    issue: &IssueRef,
) -> Result<Option<SupervisionRecord>, MemoryError> {
    let Some(drawer) = read_current(db, &supervision_key(issue))? else {
        return Ok(None);
    };
    let body: SupervisionBody = serde_json::from_str(&drawer.content)?;
    Ok(Some(SupervisionRecord {
        issue: IssueRef::new(body.repo, body.issue),
        fingerprint: body.fingerprint,
        progress_observed_at: body.progress_observed_at,
        first_absent_at: body.first_absent_at,
        last_checked_at: body.last_checked_at,
        active_redirect: body.active_redirect,
        redirect_signature: body.redirect_signature,
        redirect_issued_after_attempts: body.redirect_issued_after_attempts,
        escalated_signature: body.escalated_signature,
    }))
}

/// The strategy redirect in force for an issue's next dispatch, if any.
///
/// [`super::run::run_issue`] calls this to fill rung 2's
/// [`super::turn_prompt::TurnPromptInputs::strategy_redirect`] slot, which
/// was provisioned for this and hardcoded to `None` until now.
pub fn active_redirect(db: &Database, issue: &IssueRef) -> Result<Option<String>, MemoryError> {
    Ok(get_supervision(db, issue)?.and_then(|record| record.active_redirect))
}

/// The failure signature an issue has been escalated on, if any.
///
/// [`super::run::run_issue`] calls this *before* dispatching. Without it the
/// escalation would be a report nobody acts on, and the issue would keep
/// spending its remaining attempts on the approach supervision already
/// judged doomed — the spec's "never silent infinite retry", silently
/// retried.
pub fn escalated_signature(db: &Database, issue: &IssueRef) -> Result<Option<String>, MemoryError> {
    Ok(get_supervision(db, issue)?.and_then(|record| record.escalated_signature))
}

/// Clear an issue's escalation so work can resume, as a human re-labeling an
/// `agent:exhausted` issue does.
///
/// Clears the redirect alongside it: resuming an issue while still telling
/// the IC not to repeat a failure it is about to be re-escalated on would
/// re-escalate on the very next attempt.
pub fn clear_escalation(db: &Database, issue: &IssueRef) -> Result<bool, MemoryError> {
    let Some(mut record) = get_supervision(db, issue)? else {
        return Ok(false);
    };
    if record.escalated_signature.is_none() {
        return Ok(false);
    }
    record.escalated_signature = None;
    record.active_redirect = None;
    record.redirect_signature = None;
    record.redirect_issued_after_attempts = None;
    upsert_supervision(db, &record)?;
    Ok(true)
}

/// Clear an issue's active redirect once it is no longer relevant — the
/// issue succeeded, or a *different* failure signature took over.
///
/// Keeps the escalation history: `escalated_signature` is deliberately not
/// touched, so a redirect that has already been escalated on cannot be
/// silently re-armed by clearing.
pub fn clear_redirect(db: &Database, issue: &IssueRef) -> Result<bool, MemoryError> {
    let Some(mut record) = get_supervision(db, issue)? else {
        return Ok(false);
    };
    if record.active_redirect.is_none() {
        return Ok(false);
    }
    record.active_redirect = None;
    record.redirect_signature = None;
    record.redirect_issued_after_attempts = None;
    upsert_supervision(db, &record)?;
    Ok(true)
}

// ── progress ────────────────────────────────────────────────────────────

/// The observable state of one issue, as one comparable string.
///
/// Composed from the two things a supervisor outside the IC's process can
/// actually see: the dispatch-state drawer (which [`super::run::run_issue`]
/// rewrites at every transition) and the lineage. Any difference is progress.
///
/// Format is intentionally readable rather than hashed — this string is
/// persisted and shown in a supervision report, and "which field moved" is
/// the first question anyone debugging a false death will ask.
pub fn progress_fingerprint(
    state: Option<&DispatchState>,
    attempts: &[AttemptRecord],
    status: Option<&IssueStatus>,
) -> String {
    let dispatch = match state {
        Some(s) => format!(
            "state={} attempt={} turn={} claimed={}",
            s.state, s.attempt_n, s.turn_n, s.session_claimed
        ),
        None => "state=none".to_string(),
    };
    let lineage = format!(
        "attempts={} last={}",
        attempts.len(),
        attempts.last().map(|a| a.attempt_n).unwrap_or(0)
    );
    let issue_status = match status {
        Some(s) => format!(
            "cumulative={} best={}",
            s.cumulative_attempt_n,
            match s.best_verdict {
                Some(AttemptOutcome::Success) => "success",
                Some(AttemptOutcome::Failed) => "failed",
                None => "none",
            }
        ),
        None => "cumulative=0 best=none".to_string(),
    };
    format!("{dispatch} | {lineage} | {issue_status}")
}

/// Read the current fingerprint for an issue straight from storage.
pub fn observe_progress(db: &Database, issue: &IssueRef) -> Result<String, MemoryError> {
    let state = dispatch_state::get_dispatch_state(db, issue)?;
    let attempts = lineage::attempts_for_issue(db, issue)?;
    let status = lineage::get_issue_status(db, issue)?;
    Ok(progress_fingerprint(
        state.as_ref(),
        &attempts,
        status.as_ref(),
    ))
}

// ── process-health ──────────────────────────────────────────────────────

/// Which of the two death conditions spared an absent IC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SilentReason {
    /// The observable state moved inside the progress window. This is the
    /// spec's named false-positive guard: "IC silent but checkpointing → NOT
    /// declared dead."
    ProgressAdvancedRecently,
    /// The session has not been missing long enough yet.
    WithinLivenessGrace,
}

/// The answer to "is the IC alive and making progress?".
///
/// Note the question has two halves, and so does this enum: listedness alone
/// answers only the first.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "health", rename_all = "snake_case")]
pub enum ProcessHealth {
    /// The registry lists the session and the observable state is moving.
    Healthy,
    /// The registry lists the session, but nothing observable has changed for
    /// longer than the progress window.
    ///
    /// **Not** a death, and deliberately not actionable. The spec's rule is
    /// that death requires an *unanswered* ping, and a listed session has
    /// answered; killing or restarting one would risk running two ICs against
    /// the same worktree.
    ///
    /// It exists because rung 7 measured that `claude agents --json` lists
    /// background sessions weeks old, so listedness is a weaker statement
    /// than "working" — and reporting such a session as flatly `Healthy`
    /// would make process-health a check that can never fail on its most
    /// likely real failure. Reported, bounded by the attempt cap and
    /// strategy-health, and left to a human. See [`super::registry`]'s
    /// measurement note.
    AliveButStalled { stalled_for_secs: u64 },
    /// The registry answered and did not list the session, but at least one
    /// of the two death conditions is unmet.
    SilentNotDead {
        absent_for_secs: u64,
        stalled_for_secs: u64,
        reason: SilentReason,
    },
    /// Absent past the grace **and** stalled past the window. Both, never
    /// either.
    Dead {
        absent_for_secs: u64,
        stalled_for_secs: u64,
    },
    /// The registry could not be read, or a stored clock could not be parsed.
    /// Never an input to any action.
    Unknown { reason: String },
}

/// Elapsed whole seconds from `then` to `now`, or `None` if `then` is
/// unparseable.
///
/// A clock that runs backwards (a stored timestamp in the future, e.g. after
/// a system clock correction) saturates at zero rather than wrapping, so the
/// worst a bad clock can do is make an IC look *younger* — never older, which
/// is the direction that would kill it.
fn elapsed_secs(then: &str, now: DateTime<Utc>) -> Option<u64> {
    let parsed = DateTime::parse_from_rfc3339(then).ok()?;
    let delta = now.signed_duration_since(parsed.with_timezone(&Utc));
    Some(delta.num_seconds().max(0) as u64)
}

/// The spec's process-health check.
///
/// `prior` is the issue's supervision record as of the *previous* check —
/// this is what supplies the two clocks' origins. `first_absent_at` is passed
/// separately because the caller has already reconciled it against this
/// check's liveness reading (see [`supervise_issue`]).
pub fn assess_process_health(
    liveness: Liveness,
    progress_observed_at: &str,
    first_absent_at: Option<&str>,
    now: DateTime<Utc>,
    config: &SupervisionConfig,
) -> ProcessHealth {
    match liveness {
        Liveness::Unknown => ProcessHealth::Unknown {
            reason: "the session registry could not be read; liveness is unknown, which is not \
                     the same as absent"
                .to_string(),
        },
        Liveness::Alive => match elapsed_secs(progress_observed_at, now) {
            Some(stalled_for_secs) if stalled_for_secs >= config.progress_window_secs => {
                ProcessHealth::AliveButStalled { stalled_for_secs }
            }
            Some(_) => ProcessHealth::Healthy,
            // An unparseable progress timestamp cannot be turned into a
            // stall. Reported as unknown rather than assumed healthy, for
            // the same reason the absent branch does it.
            None => ProcessHealth::Unknown {
                reason: "a stored supervision timestamp could not be parsed".to_string(),
            },
        },
        Liveness::NotListed => {
            let Some(absent_at) = first_absent_at else {
                // The caller must set this on any NotListed reading. Reaching
                // here means it did not, and inventing an origin would start
                // both clocks at zero-elapsed — indistinguishable from a
                // freshly-absent IC, but for the wrong reason.
                return ProcessHealth::Unknown {
                    reason: "no absence timestamp recorded for a session reported missing"
                        .to_string(),
                };
            };
            let (Some(absent_for_secs), Some(stalled_for_secs)) = (
                elapsed_secs(absent_at, now),
                elapsed_secs(progress_observed_at, now),
            ) else {
                return ProcessHealth::Unknown {
                    reason: "a stored supervision timestamp could not be parsed".to_string(),
                };
            };

            // The AND, and the order it is reported in. Progress is checked
            // first only so the spec's named guard is the reason given when
            // both would apply.
            if stalled_for_secs < config.progress_window_secs {
                return ProcessHealth::SilentNotDead {
                    absent_for_secs,
                    stalled_for_secs,
                    reason: SilentReason::ProgressAdvancedRecently,
                };
            }
            if absent_for_secs < config.liveness_grace_secs {
                return ProcessHealth::SilentNotDead {
                    absent_for_secs,
                    stalled_for_secs,
                    reason: SilentReason::WithinLivenessGrace,
                };
            }
            ProcessHealth::Dead {
                absent_for_secs,
                stalled_for_secs,
            }
        }
    }
}

// ── strategy-health ─────────────────────────────────────────────────────

/// The answer to "is it alive but thrashing the same failure?".
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "strategy", rename_all = "snake_case")]
pub enum StrategyHealth {
    /// No repeated-failure pattern in the trailing run of failed attempts.
    Ok,
    /// The last `consecutive` failed attempts all failed the same way.
    Thrashing { signature: String, consecutive: u32 },
}

/// The attempts that are actual tries, excluding attempt-cap terminal
/// markers.
///
/// One definition, used by both the thrash window and the redirect-delivery
/// check, so the two cannot disagree about what counts as an attempt — which
/// would make escalation fire a try early or a try late.
fn real_attempts_of(attempts: &[AttemptRecord]) -> Vec<&AttemptRecord> {
    attempts
        .iter()
        .filter(|a| !super::run::is_terminal_summary(&a.approach))
        .collect()
}

/// How many of `attempts` are actual tries. See [`real_attempts_of`].
pub fn real_attempts(attempts: &[AttemptRecord]) -> usize {
    real_attempts_of(attempts).len()
}

/// Normalize one attempt's `why_failed` into a comparable signature.
///
/// Case-folded and whitespace-collapsed, and nothing more. The temptation is
/// to normalize harder — strip line numbers, paths, timings — so that "nearly
/// the same" failures match. That is deliberately not done: the spec asks for
/// *identical* failures ("feed identical failure repeatedly"), and every
/// additional normalization widens what counts as thrashing, which ends in
/// escalating an issue whose failures were genuinely moving.
fn failure_signature(why_failed: &str) -> String {
    let collapsed = why_failed.split_whitespace().collect::<Vec<_>>().join(" ");
    scrub_and_bound(&collapsed.to_lowercase(), MAX_SIGNATURE_CHARS).text
}

/// The spec's strategy-health check, over an issue's lineage.
///
/// Only the **trailing run** of failed attempts counts: a success anywhere
/// resets the pattern, because an approach that worked once is evidence the
/// IC is not stuck. Attempt-cap terminal markers are excluded — they are
/// summaries of prior attempts, not attempts, and counting one would let a
/// single real failure plus its own summary look like a repetition.
///
/// An attempt that failed with no recorded reason contributes no signature
/// and therefore cannot thrash. That fails toward "not thrashing" on purpose:
/// thrashing escalates, escalation stops work, and stopping work on the
/// strength of an *absent* diagnostic would be concluding from nothing. The
/// attempt cap still bounds the issue in the meantime.
pub fn assess_strategy_health(
    attempts: &[AttemptRecord],
    config: &SupervisionConfig,
) -> StrategyHealth {
    let real = real_attempts_of(attempts);

    let mut trailing: Vec<String> = Vec::new();
    for attempt in real.iter().rev() {
        if attempt.verdict == AttemptOutcome::Success {
            break;
        }
        let Some(why) = attempt.why_failed.as_deref() else {
            break;
        };
        let signature = failure_signature(why);
        if signature.is_empty() {
            break;
        }
        trailing.push(signature);
    }

    let threshold = config.thrash_threshold as usize;
    if trailing.len() < threshold {
        return StrategyHealth::Ok;
    }
    let newest = &trailing[0];
    let consecutive = trailing.iter().take_while(|s| *s == newest).count();
    if consecutive >= threshold {
        return StrategyHealth::Thrashing {
            signature: newest.clone(),
            consecutive: consecutive as u32,
        };
    }
    StrategyHealth::Ok
}

/// The redirect text handed to the next dispatch's turn prompt.
///
/// Mechanically generated, and says so — see the module doc's OQ9 note. It can
/// name the repeated failure and forbid repeating the approach; it cannot
/// propose the better approach a Lead with cross-repo context might.
pub fn redirect_text(signature: &str, consecutive: u32) -> String {
    let quoted = scrub_and_bound(signature, MAX_SIGNATURE_CHARS / 2).text;
    let text = format!(
        "STRATEGY REDIRECT (issued automatically by supervision, not by a human): your last \
         {consecutive} attempts on this issue all failed the same way — \"{quoted}\". Whatever \
         you have been doing is not converging. Do NOT repeat it. Before writing any code, \
         state in one line what you now believe the actual cause is and why it differs from \
         your previous attempts; if you cannot name a genuinely different approach, report the \
         verdict as impossible rather than trying the same thing again."
    );
    scrub_and_bound(&text, MAX_REDIRECT_CHARS).text
}

// ── the combined check ──────────────────────────────────────────────────

/// What supervision decided to do about an issue.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum SupervisionAction {
    /// Nothing to do.
    None,
    /// Neither check could reach a conclusion. Hold; act on nothing.
    Hold { reason: String },
    /// The IC is dead by the spec's two-clock rule. The next
    /// `ironmem autopilot run` resumes its session from the last checkpoint.
    RestartFromCheckpoint {
        absent_for_secs: u64,
        stalled_for_secs: u64,
    },
    /// A redirect was newly issued and is now in force for the next dispatch.
    Redirect { signature: String },
    /// The same failure signature failed again *after* a dispatch ran with a
    /// redirect in force. Stop and tell a human.
    ///
    /// "Stop" is literal, not advisory: [`escalated_signature`] refuses to
    /// dispatch the issue again, and [`super::run::run_issue`] terminates
    /// with [`super::run::TerminalReason::StrategyEscalated`] rather than
    /// burning the remaining attempts on an approach supervision has already
    /// judged doomed. Like `agent:exhausted`, it never self-resumes — a human
    /// clears it with `ironmem autopilot supervise --clear-escalation`.
    Escalate { signature: String, reason: String },
}

/// One supervision pass over one issue.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SupervisionReport {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub process: ProcessHealth,
    pub strategy: StrategyHealth,
    pub action: SupervisionAction,
    /// Whether a dispatch-state drawer exists at all. `false` means the issue
    /// is not in flight, which makes process-health vacuous — an IC that
    /// finished is *supposed* to be gone.
    pub in_flight: bool,
}

/// See [`super::run`]'s note on why `IssueRef` has no `Serialize` impl of its
/// own.
fn serialize_issue<S>(issue: &IssueRef, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    use serde::ser::SerializeStruct;
    let mut s = serializer.serialize_struct("IssueRef", 3)?;
    s.serialize_field("repo", &issue.repo)?;
    s.serialize_field("number", &issue.number)?;
    s.serialize_field("canonical", &issue.canonical())?;
    s.end()
}

/// Choose the action, given both checks and what has already been tried.
///
/// The precedence is not arbitrary:
///
/// 1. **Escalate first.** It is the terminal outcome, and it specifically
///    means "a redirect has already been tried and did not work". Restarting
///    or re-redirecting past that would be running the IC into the same wall
///    a third time.
/// 2. **Hold beats restart.** An unreadable registry is not evidence of
///    anything; restarting on it would act on no information.
/// 3. **Restart beats redirect.** A dead process cannot read a redirect. The
///    redirect is still *persisted* (see [`supervise_issue`]) and the
///    restarted dispatch picks it up from the drawer, so nothing is lost by
///    reporting the more urgent action.
///
/// `real_attempt_count` is the number of non-marker attempts on the issue
/// *now*. It is what distinguishes "the redirect failed" from "we were polled
/// again": escalation requires the count to have moved past
/// [`SupervisionRecord::redirect_issued_after_attempts`], so a supervisor run
/// on a cron cannot escalate a redirect no dispatch has yet read.
pub fn plan_supervision(
    process: &ProcessHealth,
    strategy: &StrategyHealth,
    prior: Option<&SupervisionRecord>,
    real_attempt_count: u32,
) -> SupervisionAction {
    if let StrategyHealth::Thrashing {
        signature,
        consecutive,
    } = strategy
    {
        // An escalation already recorded for this signature stands: it is a
        // stable state a poll loop re-reads, not a new event each pass.
        let already_escalated = prior
            .and_then(|r| r.escalated_signature.as_deref())
            .is_some_and(|s| s == signature);
        // A redirect counts as *tried* only once a dispatch has appended a
        // new attempt since it was issued.
        let redirect_was_tried = prior.is_some_and(|r| {
            r.redirect_signature.as_deref() == Some(signature.as_str())
                && r.redirect_issued_after_attempts
                    .is_some_and(|at| real_attempt_count > at)
        });
        if already_escalated || redirect_was_tried {
            return SupervisionAction::Escalate {
                signature: signature.clone(),
                reason: format!(
                    "{consecutive} consecutive attempts failed identically, and at least one of \
                     them ran with a strategy redirect already in force for this exact failure — \
                     the redirect did not change the outcome"
                ),
            };
        }
    }
    match process {
        ProcessHealth::Unknown { reason } => {
            return SupervisionAction::Hold {
                reason: reason.clone(),
            }
        }
        ProcessHealth::Dead {
            absent_for_secs,
            stalled_for_secs,
        } => {
            return SupervisionAction::RestartFromCheckpoint {
                absent_for_secs: *absent_for_secs,
                stalled_for_secs: *stalled_for_secs,
            }
        }
        ProcessHealth::AliveButStalled { stalled_for_secs } => {
            // Reported, never acted on: see the variant's doc. A thrashing
            // issue still reaches the redirect arm below, because a stalled
            // *process* and a doomed *strategy* are independent facts.
            if matches!(strategy, StrategyHealth::Ok) {
                return SupervisionAction::Hold {
                    reason: format!(
                        "the session is still listed but nothing observable has changed in \
                         {stalled_for_secs}s; a listed session has answered, so it is not dead \
                         and must not be restarted — this needs a human's eye"
                    ),
                };
            }
        }
        ProcessHealth::Healthy | ProcessHealth::SilentNotDead { .. } => {}
    }
    if let StrategyHealth::Thrashing { signature, .. } = strategy {
        return SupervisionAction::Redirect {
            signature: signature.clone(),
        };
    }
    SupervisionAction::None
}

/// Run both checks against one issue and persist the result.
///
/// Reads the registry snapshot rather than taking a registry, so a caller
/// supervising several issues reads the registry once — and so every issue in
/// one pass sees the *same* liveness answer, which a per-issue read could not
/// guarantee.
pub fn supervise_issue(
    db: &Database,
    issue: &IssueRef,
    snapshot: &RegistrySnapshot,
    config: &SupervisionConfig,
) -> Result<SupervisionReport, MemoryError> {
    validate_repo(&issue.repo)?;
    config.validate()?;
    let now = Utc::now();
    let now_str = now.to_rfc3339();

    let state = dispatch_state::get_dispatch_state(db, issue)?;
    let attempts = lineage::attempts_for_issue(db, issue)?;
    let status = lineage::get_issue_status(db, issue)?;
    let fingerprint = progress_fingerprint(state.as_ref(), &attempts, status.as_ref());

    let prior = get_supervision(db, issue)?;

    // The progress clock's origin: unchanged if the fingerprint is unchanged,
    // otherwise now. A first-ever check has no prior fingerprint to differ
    // from, so its origin is now — which is what makes "the first check can
    // never declare an IC dead" true by construction rather than by a guard.
    let progress_observed_at = match &prior {
        Some(record) if record.fingerprint == fingerprint => record.progress_observed_at.clone(),
        _ => now_str.clone(),
    };

    // Liveness is only a meaningful question for an issue that is supposed to
    // have a process. An IC exits at the end of every dispatch by design, so
    // an issue with no dispatch-state drawer being absent from the registry is
    // the normal finished state, not a death.
    let liveness = match &state {
        Some(state) => snapshot.liveness(&state.ic_session_name),
        None => Liveness::Alive,
    };

    // The liveness clock's origin. Note the `Unknown` arm: an unreadable
    // registry neither starts nor clears the absence clock, because it is not
    // an observation of absence or of presence.
    let first_absent_at = match liveness {
        Liveness::Alive => None,
        Liveness::NotListed => Some(
            prior
                .as_ref()
                .and_then(|r| r.first_absent_at.clone())
                .unwrap_or_else(|| now_str.clone()),
        ),
        Liveness::Unknown => prior.as_ref().and_then(|r| r.first_absent_at.clone()),
    };

    let process = if state.is_some() {
        assess_process_health(
            liveness,
            &progress_observed_at,
            first_absent_at.as_deref(),
            now,
            config,
        )
    } else {
        ProcessHealth::Healthy
    };
    let strategy = assess_strategy_health(&attempts, config);
    let real_attempt_count = real_attempts(&attempts) as u32;
    let action = plan_supervision(&process, &strategy, prior.as_ref(), real_attempt_count);

    // Persist the interventions. A redirect is armed whenever one is called
    // for, independently of which action was *reported* — see
    // `plan_supervision`'s precedence note — so a restarted dispatch still
    // reads it. Both are recorded per signature, so a poll loop calling this
    // every minute issues each exactly once.
    let mut active_redirect = prior.as_ref().and_then(|r| r.active_redirect.clone());
    let mut redirect_signature = prior.as_ref().and_then(|r| r.redirect_signature.clone());
    let mut redirect_issued_after_attempts = prior
        .as_ref()
        .and_then(|r| r.redirect_issued_after_attempts);
    let mut escalated_signature = prior.as_ref().and_then(|r| r.escalated_signature.clone());
    match (&action, &strategy) {
        (SupervisionAction::Escalate { signature, .. }, _) => {
            escalated_signature = Some(signature.clone());
        }
        (
            SupervisionAction::Redirect { .. } | SupervisionAction::RestartFromCheckpoint { .. },
            StrategyHealth::Thrashing {
                signature,
                consecutive,
            },
        ) if redirect_signature.as_deref() != Some(signature.as_str()) => {
            active_redirect = Some(redirect_text(signature, *consecutive));
            redirect_signature = Some(signature.clone());
            redirect_issued_after_attempts = Some(real_attempt_count);
        }
        _ => {}
    }
    // A redirect whose signature is no longer the one failing has served its
    // purpose (or been overtaken). Leaving it in force would keep telling the
    // IC not to repeat a failure it has already stopped repeating.
    if let StrategyHealth::Ok = strategy {
        if active_redirect.is_some() && !attempts.is_empty() {
            let still_relevant = attempts
                .iter()
                .rev()
                .find(|a| !super::run::is_terminal_summary(&a.approach))
                .and_then(|a| a.why_failed.as_deref())
                .map(|why| Some(failure_signature(why)) == redirect_signature)
                .unwrap_or(false);
            if !still_relevant {
                active_redirect = None;
                redirect_signature = None;
                redirect_issued_after_attempts = None;
            }
        }
    }

    let record = SupervisionRecord {
        issue: issue.clone(),
        fingerprint,
        progress_observed_at,
        first_absent_at,
        last_checked_at: now_str,
        active_redirect,
        redirect_signature,
        redirect_issued_after_attempts,
        escalated_signature,
    };
    upsert_supervision(db, &record)?;

    Ok(SupervisionReport {
        issue: issue.clone(),
        process,
        strategy,
        action,
        in_flight: state.is_some(),
    })
}

// ── reconciliation on Lead restart ──────────────────────────────────────

/// One row of the spec's *Lead crash-safe state* restart table.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum ReconcileVerdict {
    /// Drawer present, session alive → adopt and resume supervision.
    Adopt,
    /// Drawer present, session gone → restart from checkpoint (or quarantine
    /// the worktree, which [`super::worktree::ensure_worktree`] already does
    /// for a dirty checkout on the next run).
    ///
    /// `session_claimed` distinguishes the two shapes of this row, and the
    /// distinction is rung 4's HIGH finding: a drawer written before the
    /// first launch names a session uuid no `claude` process has ever seen,
    /// so there is no checkpoint to restart *from* — the next run must open
    /// the session rather than resume it.
    RestartFromCheckpoint { session_claimed: bool },
    /// Session alive, no drawer → **flag for a human. Never silently
    /// adopted.** Autopilot cannot know what this IC was told to do, and
    /// adopting it would mean supervising work whose issue, class and budget
    /// are all unknown.
    Orphan,
    /// The registry could not be read. Nothing is concluded.
    Hold { reason: String },
}

/// One reconciled entity: an in-flight issue, an orphaned session, or both.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reconciliation {
    /// `None` for an orphan — the whole point of an orphan is that the issue
    /// it belongs to is not recorded anywhere.
    #[serde(serialize_with = "serialize_optional_issue")]
    pub issue: Option<IssueRef>,
    pub session_name: String,
    pub verdict: ReconcileVerdict,
}

fn serialize_optional_issue<S>(issue: &Option<IssueRef>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match issue {
        Some(issue) => serialize_issue(issue, serializer),
        None => serializer.serialize_none(),
    }
}

/// Rebuild the Lead's picture of the world from drawers, reconciled against
/// the session registry.
///
/// The spec's own words on why this exists: `ListAgents` "reveals only who is
/// *alive*, not what the Lead *knew*". The drawers hold what it knew; the
/// registry holds who is alive; this is the join, and every one of its three
/// rows is a different action.
///
/// Read-only by design. It reports verdicts and changes nothing — restarting
/// an issue is `ironmem autopilot run`, which is idempotent and already knows
/// how to resume a session and quarantine a dirty worktree. A reconciler that
/// also acted would be a second dispatch path with none of that machinery.
pub fn reconcile(
    db: &Database,
    snapshot: &RegistrySnapshot,
) -> Result<Vec<Reconciliation>, MemoryError> {
    let states = dispatch_state::all_dispatch_states(db, MAX_IN_FLIGHT_DISPATCH_STATES)?;
    let mut out = Vec::new();
    let mut known_sessions = std::collections::HashSet::new();

    for state in &states {
        known_sessions.insert(state.ic_session_name.clone());
        let verdict = match snapshot.liveness(&state.ic_session_name) {
            Liveness::Alive => ReconcileVerdict::Adopt,
            Liveness::NotListed => ReconcileVerdict::RestartFromCheckpoint {
                session_claimed: state.session_claimed,
            },
            Liveness::Unknown => ReconcileVerdict::Hold {
                reason: match snapshot {
                    RegistrySnapshot::Unavailable { reason } => reason.clone(),
                    // Unreachable: an Available snapshot never answers
                    // Unknown. Spelled out rather than unwrapped so a future
                    // change to `liveness` cannot turn this into a panic.
                    RegistrySnapshot::Available(_) => "liveness unavailable".to_string(),
                },
            },
        };
        out.push(Reconciliation {
            issue: Some(state.issue.clone()),
            session_name: state.ic_session_name.clone(),
            verdict,
        });
    }

    // Orphan detection is only sound against a registry that actually
    // answered. From an unreadable one, "this session has no drawer" cannot
    // be distinguished from "we could not see any sessions at all", and the
    // spec's orphan row calls for flagging a human — which would then mean
    // paging a human every time the registry hiccuped.
    if snapshot.is_available() {
        for name in snapshot.names() {
            if name.starts_with(IC_SESSION_PREFIX) && !known_sessions.contains(name) {
                out.push(Reconciliation {
                    issue: None,
                    session_name: name.to_string(),
                    verdict: ReconcileVerdict::Orphan,
                });
            }
        }
    }

    Ok(out)
}

/// The prefix [`super::dispatch::ic_name`] gives every IC session, and the
/// only thing that distinguishes an Autopilot IC from the human's own
/// sessions in a shared registry. Sessions without it are not Autopilot's
/// business and are never reported as orphans.
pub const IC_SESSION_PREFIX: &str = "ic-";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::registry::AgentEntry;

    fn issue() -> IssueRef {
        IssueRef::new("ironrace/ironmem", 283)
    }

    fn now() -> DateTime<Utc> {
        Utc::now()
    }

    fn ago(secs: i64) -> String {
        (Utc::now() - chrono::Duration::seconds(secs)).to_rfc3339()
    }

    fn state_for(issue: &IssueRef, claimed: bool) -> DispatchState {
        DispatchState {
            issue: issue.clone(),
            worktree_path: "/tmp/wt".into(),
            ic_session_name: super::super::dispatch::ic_name(issue),
            dispatch_class: "logic".into(),
            attempt_n: 1,
            state: "dispatching".into(),
            started_at: ago(600),
            session_uuid: "11111111-1111-1111-1111-111111111111".into(),
            turn_n: 3,
            session_claimed: claimed,
        }
    }

    fn attempt(n: u32, verdict: AttemptOutcome, why: Option<&str>) -> AttemptRecord {
        AttemptRecord {
            issue: issue(),
            attempt_n: n,
            approach: format!("approach {n}"),
            verdict,
            why_failed: why.map(|s| s.to_string()),
            commit_sha: None,
        }
    }

    fn alive_snapshot(issue: &IssueRef) -> RegistrySnapshot {
        RegistrySnapshot::Available(vec![AgentEntry {
            name: super::super::dispatch::ic_name(issue),
            status: None,
        }])
    }

    // ── config ──────────────────────────────────────────────────────────

    #[test]
    fn default_config_is_valid_and_its_clocks_are_ordered() {
        let config = SupervisionConfig::default();
        assert!(config.validate().is_ok());
        assert!(config.liveness_grace_secs < config.progress_window_secs);
    }

    #[test]
    fn equal_clocks_are_refused_because_the_and_would_collapse() {
        let config = SupervisionConfig {
            liveness_grace_secs: 300,
            progress_window_secs: 300,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_zero_grace_is_refused() {
        let config = SupervisionConfig {
            liveness_grace_secs: 0,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_thrash_threshold_below_two_is_refused() {
        let config = SupervisionConfig {
            thrash_threshold: 1,
            ..Default::default()
        };
        assert!(config.validate().is_err());
    }

    // ── process-health: the AND, from both sides ────────────────────────

    #[test]
    fn a_listed_session_that_is_moving_is_healthy() {
        let health = assess_process_health(
            Liveness::Alive,
            &ago(5),
            Some(&ago(100_000)),
            now(),
            &SupervisionConfig::default(),
        );
        assert_eq!(
            health,
            ProcessHealth::Healthy,
            "a long-past absence timestamp is irrelevant once the session is listed again"
        );
    }

    #[test]
    fn a_listed_session_is_never_dead_however_stale() {
        // The one thing listedness *does* settle: the ping was answered, so
        // the spec's death rule cannot be satisfied.
        for progress in [ago(1), ago(100_000)] {
            let health = assess_process_health(
                Liveness::Alive,
                &progress,
                Some(&ago(100_000)),
                now(),
                &SupervisionConfig::default(),
            );
            assert!(!matches!(health, ProcessHealth::Dead { .. }));
        }
    }

    #[test]
    fn a_listed_but_frozen_session_is_reported_rather_than_called_healthy() {
        // Rung 7 measured that `claude agents --json` lists background
        // sessions weeks old. If listedness alone meant healthy,
        // process-health could never fail on its most likely real failure.
        let health = assess_process_health(
            Liveness::Alive,
            &ago(100_000),
            None,
            now(),
            &SupervisionConfig::default(),
        );
        assert_eq!(
            health,
            ProcessHealth::AliveButStalled {
                stalled_for_secs: 100_000
            }
        );
    }

    #[test]
    fn a_listed_and_frozen_session_is_never_restarted() {
        // The spec's death rule needs an *unanswered* ping. A listed session
        // has answered, so restarting it risks two ICs on one worktree.
        let action = plan_supervision(
            &ProcessHealth::AliveButStalled {
                stalled_for_secs: 100_000,
            },
            &StrategyHealth::Ok,
            None,
            0,
        );
        assert!(
            matches!(action, SupervisionAction::Hold { .. }),
            "got {action:?}"
        );
    }

    #[test]
    fn a_stalled_but_listed_session_still_gets_a_strategy_redirect() {
        // A frozen process and a doomed strategy are independent facts; the
        // hold on the first must not suppress the intervention for the
        // second.
        let action = plan_supervision(
            &ProcessHealth::AliveButStalled {
                stalled_for_secs: 100_000,
            },
            &StrategyHealth::Thrashing {
                signature: "same".into(),
                consecutive: 3,
            },
            None,
            3,
        );
        assert!(
            matches!(action, SupervisionAction::Redirect { .. }),
            "got {action:?}"
        );
    }

    #[test]
    fn absent_and_stalled_past_both_thresholds_is_dead() {
        let health = assess_process_health(
            Liveness::NotListed,
            &ago(2_000),
            Some(&ago(2_000)),
            now(),
            &SupervisionConfig::default(),
        );
        assert!(matches!(health, ProcessHealth::Dead { .. }));
    }

    #[test]
    fn absent_but_progressing_is_not_dead() {
        // The spec's named false-positive guard: "IC silent but checkpointing
        // → NOT declared dead".
        let health = assess_process_health(
            Liveness::NotListed,
            &ago(5),
            Some(&ago(10_000)),
            now(),
            &SupervisionConfig::default(),
        );
        assert_eq!(
            health,
            ProcessHealth::SilentNotDead {
                absent_for_secs: 10_000,
                stalled_for_secs: 5,
                reason: SilentReason::ProgressAdvancedRecently,
            }
        );
    }

    #[test]
    fn stalled_but_only_briefly_absent_is_not_dead() {
        let health = assess_process_health(
            Liveness::NotListed,
            &ago(10_000),
            Some(&ago(5)),
            now(),
            &SupervisionConfig::default(),
        );
        assert!(matches!(
            health,
            ProcessHealth::SilentNotDead {
                reason: SilentReason::WithinLivenessGrace,
                ..
            }
        ));
    }

    #[test]
    fn an_unreadable_registry_is_unknown_never_dead() {
        let health = assess_process_health(
            Liveness::Unknown,
            &ago(100_000),
            Some(&ago(100_000)),
            now(),
            &SupervisionConfig::default(),
        );
        assert!(matches!(health, ProcessHealth::Unknown { .. }));
    }

    #[test]
    fn an_unparseable_stored_timestamp_is_unknown_not_infinitely_stale() {
        // The dangerous reading of a bad timestamp is "epoch, therefore
        // stalled forever, therefore dead".
        let health = assess_process_health(
            Liveness::NotListed,
            "not-a-timestamp",
            Some(&ago(10_000)),
            now(),
            &SupervisionConfig::default(),
        );
        assert!(matches!(health, ProcessHealth::Unknown { .. }));
    }

    #[test]
    fn a_future_timestamp_saturates_at_zero_rather_than_wrapping() {
        let future = (Utc::now() + chrono::Duration::seconds(10_000)).to_rfc3339();
        let health = assess_process_health(
            Liveness::NotListed,
            &future,
            Some(&future),
            now(),
            &SupervisionConfig::default(),
        );
        // Saturated to zero elapsed on both clocks → nowhere near dead.
        assert!(matches!(health, ProcessHealth::SilentNotDead { .. }));
    }

    #[test]
    fn a_missing_absence_timestamp_is_unknown_not_a_fresh_clock() {
        let health = assess_process_health(
            Liveness::NotListed,
            &ago(10_000),
            None,
            now(),
            &SupervisionConfig::default(),
        );
        assert!(matches!(health, ProcessHealth::Unknown { .. }));
    }

    // ── strategy-health ─────────────────────────────────────────────────

    #[test]
    fn three_identical_failures_are_thrashing() {
        let attempts = vec![
            attempt(1, AttemptOutcome::Failed, Some("assertion failed: x == 1")),
            attempt(2, AttemptOutcome::Failed, Some("assertion failed: x == 1")),
            attempt(3, AttemptOutcome::Failed, Some("assertion failed: x == 1")),
        ];
        match assess_strategy_health(&attempts, &SupervisionConfig::default()) {
            StrategyHealth::Thrashing { consecutive, .. } => assert_eq!(consecutive, 3),
            other => panic!("expected Thrashing, got {other:?}"),
        }
    }

    #[test]
    fn case_and_whitespace_differences_still_count_as_identical() {
        let attempts = vec![
            attempt(
                1,
                AttemptOutcome::Failed,
                Some("Assertion   failed: X == 1"),
            ),
            attempt(2, AttemptOutcome::Failed, Some("assertion failed: x == 1")),
            attempt(
                3,
                AttemptOutcome::Failed,
                Some("ASSERTION FAILED:  x == 1\n"),
            ),
        ];
        assert!(matches!(
            assess_strategy_health(&attempts, &SupervisionConfig::default()),
            StrategyHealth::Thrashing { .. }
        ));
    }

    #[test]
    fn genuinely_different_failures_are_not_thrashing() {
        let attempts = vec![
            attempt(1, AttemptOutcome::Failed, Some("compile error in foo.rs")),
            attempt(2, AttemptOutcome::Failed, Some("test bar failed")),
            attempt(3, AttemptOutcome::Failed, Some("clippy: needless_borrow")),
        ];
        assert_eq!(
            assess_strategy_health(&attempts, &SupervisionConfig::default()),
            StrategyHealth::Ok
        );
    }

    #[test]
    fn two_identical_failures_are_a_retry_not_a_pattern() {
        let attempts = vec![
            attempt(1, AttemptOutcome::Failed, Some("same")),
            attempt(2, AttemptOutcome::Failed, Some("same")),
        ];
        assert_eq!(
            assess_strategy_health(&attempts, &SupervisionConfig::default()),
            StrategyHealth::Ok
        );
    }

    #[test]
    fn a_success_resets_the_trailing_run() {
        let attempts = vec![
            attempt(1, AttemptOutcome::Failed, Some("same")),
            attempt(2, AttemptOutcome::Failed, Some("same")),
            attempt(3, AttemptOutcome::Success, None),
            attempt(4, AttemptOutcome::Failed, Some("same")),
        ];
        assert_eq!(
            assess_strategy_health(&attempts, &SupervisionConfig::default()),
            StrategyHealth::Ok
        );
    }

    #[test]
    fn a_failure_with_no_recorded_reason_cannot_thrash() {
        let attempts = vec![
            attempt(1, AttemptOutcome::Failed, None),
            attempt(2, AttemptOutcome::Failed, None),
            attempt(3, AttemptOutcome::Failed, None),
        ];
        assert_eq!(
            assess_strategy_health(&attempts, &SupervisionConfig::default()),
            StrategyHealth::Ok
        );
    }

    #[test]
    fn an_attempt_cap_terminal_marker_is_not_counted_as_an_attempt() {
        // The marker's `why_failed` is a summary quoting every prior attempt,
        // so counting it would both add a phantom repetition and (because it
        // quotes them) risk matching their signature.
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        for n in 1..=2u32 {
            lineage::record_attempt(&db, &attempt(n, AttemptOutcome::Failed, Some("same")))
                .unwrap();
        }
        super::super::run::record_terminal_summary(
            &db,
            &issue,
            3,
            3,
            &lineage::attempts_for_issue(&db, &issue)
                .unwrap()
                .iter()
                .map(super::super::turn_prompt::PriorAttempt::from)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let attempts = lineage::attempts_for_issue(&db, &issue).unwrap();
        assert_eq!(attempts.len(), 3, "the marker is in lineage");
        assert_eq!(
            assess_strategy_health(&attempts, &SupervisionConfig::default()),
            StrategyHealth::Ok,
            "two real failures plus a marker must not read as three repetitions"
        );
    }

    // ── plan precedence ─────────────────────────────────────────────────

    /// A record with a redirect in force for `signature`, issued when the
    /// issue had `issued_after` real attempts.
    fn prior_with_redirect(signature: &str, issued_after: Option<u32>) -> SupervisionRecord {
        SupervisionRecord {
            issue: issue(),
            fingerprint: String::new(),
            progress_observed_at: ago(10),
            first_absent_at: None,
            last_checked_at: ago(10),
            active_redirect: Some("redirect".into()),
            redirect_signature: Some(signature.into()),
            redirect_issued_after_attempts: issued_after,
            escalated_signature: None,
        }
    }

    fn thrashing(signature: &str) -> StrategyHealth {
        StrategyHealth::Thrashing {
            signature: signature.into(),
            consecutive: 3,
        }
    }

    #[test]
    fn thrashing_again_after_a_redirect_was_actually_tried_escalates() {
        // Issued at 3 attempts, now at 4: a dispatch ran with the redirect
        // in force and failed the same way regardless.
        let prior = prior_with_redirect("same", Some(3));
        let action = plan_supervision(&ProcessHealth::Healthy, &thrashing("same"), Some(&prior), 4);
        assert!(matches!(action, SupervisionAction::Escalate { .. }));
    }

    #[test]
    fn being_polled_twice_is_not_the_redirect_failing() {
        // The bug this field exists to prevent: a supervisor on a cron arms
        // a redirect on one pass and, over byte-identical lineage, escalates
        // it on the next — before any dispatch has read it. The IC's one
        // steer would be spent without ever being delivered.
        let prior = prior_with_redirect("same", Some(3));
        for poll in 0..5 {
            let action =
                plan_supervision(&ProcessHealth::Healthy, &thrashing("same"), Some(&prior), 3);
            assert!(
                !matches!(action, SupervisionAction::Escalate { .. }),
                "poll {poll} escalated a redirect no dispatch has consumed"
            );
        }
    }

    #[test]
    fn a_redirect_from_before_this_field_existed_does_not_escalate_on_sight() {
        // `#[serde(default)]` → `None`. Delaying a human notification is the
        // conservative direction; firing one on no evidence is not.
        let prior = prior_with_redirect("same", None);
        let action = plan_supervision(
            &ProcessHealth::Healthy,
            &thrashing("same"),
            Some(&prior),
            99,
        );
        assert!(!matches!(action, SupervisionAction::Escalate { .. }));
    }

    #[test]
    fn a_recorded_escalation_is_stable_across_polls() {
        let mut prior = prior_with_redirect("same", Some(3));
        prior.escalated_signature = Some("same".into());
        for _ in 0..3 {
            let action =
                plan_supervision(&ProcessHealth::Healthy, &thrashing("same"), Some(&prior), 3);
            assert!(matches!(action, SupervisionAction::Escalate { .. }));
        }
    }

    #[test]
    fn a_new_signature_earns_its_own_redirect_after_an_earlier_escalation() {
        let prior = SupervisionRecord {
            issue: issue(),
            fingerprint: String::new(),
            progress_observed_at: ago(10),
            first_absent_at: None,
            last_checked_at: ago(10),
            active_redirect: None,
            redirect_signature: None,
            redirect_issued_after_attempts: None,
            escalated_signature: Some("old failure".into()),
        };
        let action = plan_supervision(
            &ProcessHealth::Healthy,
            &StrategyHealth::Thrashing {
                signature: "a different failure".into(),
                consecutive: 3,
            },
            Some(&prior),
            4,
        );
        assert!(matches!(action, SupervisionAction::Redirect { .. }));
    }

    #[test]
    fn hold_beats_restart_and_escalate_beats_both() {
        let unknown = ProcessHealth::Unknown {
            reason: "registry".into(),
        };
        assert!(matches!(
            plan_supervision(&unknown, &StrategyHealth::Ok, None, 0),
            SupervisionAction::Hold { .. }
        ));

        let dead = ProcessHealth::Dead {
            absent_for_secs: 999,
            stalled_for_secs: 999,
        };
        assert!(matches!(
            plan_supervision(&dead, &StrategyHealth::Ok, None, 0),
            SupervisionAction::RestartFromCheckpoint { .. }
        ));

        // Dead *and* already-redirected-thrashing escalates: restarting into
        // a wall we have already tried to steer away from is the one thing
        // the ordering exists to prevent.
        let prior = SupervisionRecord {
            issue: issue(),
            fingerprint: String::new(),
            progress_observed_at: ago(10),
            first_absent_at: None,
            last_checked_at: ago(10),
            active_redirect: Some("r".into()),
            redirect_signature: Some("same".into()),
            redirect_issued_after_attempts: Some(3),
            escalated_signature: None,
        };
        assert!(matches!(
            plan_supervision(
                &dead,
                &StrategyHealth::Thrashing {
                    signature: "same".into(),
                    consecutive: 4
                },
                Some(&prior),
                4,
            ),
            SupervisionAction::Escalate { .. }
        ));
    }

    // ── supervise_issue, against a real database ────────────────────────

    #[test]
    fn a_first_check_can_never_declare_an_ic_dead() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&issue, true)).unwrap();
        let snapshot = RegistrySnapshot::Available(Vec::new()); // valid, empty

        let report =
            supervise_issue(&db, &issue, &snapshot, &SupervisionConfig::default()).unwrap();
        assert!(
            matches!(report.process, ProcessHealth::SilentNotDead { .. }),
            "got {:?}",
            report.process
        );
        assert_eq!(report.action, SupervisionAction::None);
    }

    #[test]
    fn an_issue_with_no_dispatch_state_is_not_in_flight_and_is_never_dead() {
        // An IC exits at the end of every dispatch by design; absence is the
        // normal finished state, not a death.
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        let snapshot = RegistrySnapshot::Available(Vec::new());
        let report =
            supervise_issue(&db, &issue, &snapshot, &SupervisionConfig::default()).unwrap();
        assert!(!report.in_flight);
        assert_eq!(report.process, ProcessHealth::Healthy);
    }

    #[test]
    fn an_unchanged_fingerprint_preserves_the_progress_clocks_origin() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&issue, true)).unwrap();
        let snapshot = alive_snapshot(&issue);
        let config = SupervisionConfig::default();

        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        let first = get_supervision(&db, &issue).unwrap().unwrap();
        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        let second = get_supervision(&db, &issue).unwrap().unwrap();

        assert_eq!(
            first.progress_observed_at, second.progress_observed_at,
            "no observable change must not restart the progress clock"
        );
        assert_ne!(
            first.last_checked_at, second.last_checked_at,
            "but the check itself is still recorded"
        );
    }

    #[test]
    fn observable_progress_moves_the_progress_clock() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&issue, true)).unwrap();
        let snapshot = alive_snapshot(&issue);
        let config = SupervisionConfig::default();

        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        let before = get_supervision(&db, &issue).unwrap().unwrap();

        let mut moved = state_for(&issue, true);
        moved.turn_n = 9;
        dispatch_state::upsert_dispatch_state(&db, &moved).unwrap();
        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        let after = get_supervision(&db, &issue).unwrap().unwrap();

        assert_ne!(before.fingerprint, after.fingerprint);
        assert_ne!(before.progress_observed_at, after.progress_observed_at);
    }

    #[test]
    fn an_unreadable_registry_neither_starts_nor_clears_the_absence_clock() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&issue, true)).unwrap();
        let config = SupervisionConfig::default();

        // Absent once: the clock starts.
        supervise_issue(
            &db,
            &issue,
            &RegistrySnapshot::Available(Vec::new()),
            &config,
        )
        .unwrap();
        let started = get_supervision(&db, &issue)
            .unwrap()
            .unwrap()
            .first_absent_at
            .unwrap();

        // Registry then goes unreadable: the clock must neither restart nor
        // clear, and nothing may be concluded.
        let report = supervise_issue(
            &db,
            &issue,
            &RegistrySnapshot::Unavailable {
                reason: "gone".into(),
            },
            &config,
        )
        .unwrap();
        let held = get_supervision(&db, &issue).unwrap().unwrap();
        assert_eq!(held.first_absent_at.as_deref(), Some(started.as_str()));
        assert!(matches!(report.action, SupervisionAction::Hold { .. }));

        // Listed again: the clock clears.
        supervise_issue(&db, &issue, &alive_snapshot(&issue), &config).unwrap();
        assert_eq!(
            get_supervision(&db, &issue)
                .unwrap()
                .unwrap()
                .first_absent_at,
            None
        );
    }

    #[test]
    fn a_redirect_is_issued_once_and_readable_by_the_next_dispatch() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        for n in 1..=3u32 {
            lineage::record_attempt(
                &db,
                &attempt(n, AttemptOutcome::Failed, Some("same failure")),
            )
            .unwrap();
        }
        let snapshot = alive_snapshot(&issue);
        let config = SupervisionConfig::default();

        let first = supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        assert!(matches!(first.action, SupervisionAction::Redirect { .. }));
        let redirect = active_redirect(&db, &issue).unwrap().unwrap();
        assert!(redirect.contains("STRATEGY REDIRECT"));
        assert!(redirect.contains("same failure"));
        assert_eq!(
            get_supervision(&db, &issue)
                .unwrap()
                .unwrap()
                .redirect_issued_after_attempts,
            Some(3),
            "the redirect must record how much lineage existed when it was issued"
        );
    }

    #[test]
    fn polling_again_over_unchanged_lineage_does_not_burn_the_redirect() {
        // A supervisor on a cron polls far more often than dispatches
        // happen. Escalating on the second poll would spend the IC's one
        // steer without ever delivering it.
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        for n in 1..=3u32 {
            lineage::record_attempt(
                &db,
                &attempt(n, AttemptOutcome::Failed, Some("same failure")),
            )
            .unwrap();
        }
        let snapshot = alive_snapshot(&issue);
        let config = SupervisionConfig::default();

        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        for poll in 0..4 {
            let report = supervise_issue(&db, &issue, &snapshot, &config).unwrap();
            assert!(
                !matches!(report.action, SupervisionAction::Escalate { .. }),
                "poll {poll} escalated before any dispatch read the redirect"
            );
            assert!(
                active_redirect(&db, &issue).unwrap().is_some(),
                "the redirect must stay in force until it is actually tried"
            );
        }
    }

    #[test]
    fn a_further_identical_failure_after_the_redirect_escalates_and_stays_escalated() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        for n in 1..=3u32 {
            lineage::record_attempt(
                &db,
                &attempt(n, AttemptOutcome::Failed, Some("same failure")),
            )
            .unwrap();
        }
        let snapshot = alive_snapshot(&issue);
        let config = SupervisionConfig::default();
        supervise_issue(&db, &issue, &snapshot, &config).unwrap();

        // A dispatch runs with the redirect in force and fails the same way.
        lineage::record_attempt(
            &db,
            &attempt(4, AttemptOutcome::Failed, Some("same failure")),
        )
        .unwrap();

        let report = supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        assert!(
            matches!(report.action, SupervisionAction::Escalate { .. }),
            "got {:?}",
            report.action
        );
        assert_eq!(
            escalated_signature(&db, &issue).unwrap().as_deref(),
            Some("same failure")
        );

        // Stable across further polls — a state a loop re-reads, not a new
        // event each pass.
        for _ in 0..3 {
            let again = supervise_issue(&db, &issue, &snapshot, &config).unwrap();
            assert!(matches!(again.action, SupervisionAction::Escalate { .. }));
        }
    }

    #[test]
    fn clear_escalation_lets_work_resume_and_takes_the_redirect_with_it() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        for n in 1..=4u32 {
            lineage::record_attempt(
                &db,
                &attempt(n, AttemptOutcome::Failed, Some("same failure")),
            )
            .unwrap();
        }
        let snapshot = alive_snapshot(&issue);
        let config = SupervisionConfig::default();
        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        lineage::record_attempt(
            &db,
            &attempt(5, AttemptOutcome::Failed, Some("same failure")),
        )
        .unwrap();
        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        assert!(escalated_signature(&db, &issue).unwrap().is_some());

        assert!(clear_escalation(&db, &issue).unwrap());
        assert_eq!(escalated_signature(&db, &issue).unwrap(), None);
        assert_eq!(
            active_redirect(&db, &issue).unwrap(),
            None,
            "resuming while still carrying the redirect would re-escalate on the next attempt"
        );
        // Idempotent.
        assert!(!clear_escalation(&db, &issue).unwrap());
    }

    #[test]
    fn a_redirect_is_retired_once_the_failure_it_named_stops_repeating() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        for n in 1..=3u32 {
            lineage::record_attempt(
                &db,
                &attempt(n, AttemptOutcome::Failed, Some("old failure")),
            )
            .unwrap();
        }
        let snapshot = alive_snapshot(&issue);
        let config = SupervisionConfig::default();
        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        assert!(active_redirect(&db, &issue).unwrap().is_some());

        // A genuinely different failure follows: the old redirect no longer
        // describes anything that is happening.
        lineage::record_attempt(
            &db,
            &attempt(4, AttemptOutcome::Failed, Some("a brand new failure")),
        )
        .unwrap();
        supervise_issue(&db, &issue, &snapshot, &config).unwrap();
        assert_eq!(active_redirect(&db, &issue).unwrap(), None);
    }

    #[test]
    fn clear_redirect_keeps_the_escalation_history() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        upsert_supervision(
            &db,
            &SupervisionRecord {
                issue: issue.clone(),
                fingerprint: "f".into(),
                progress_observed_at: ago(10),
                first_absent_at: None,
                last_checked_at: ago(10),
                active_redirect: Some("r".into()),
                redirect_signature: Some("sig".into()),
                redirect_issued_after_attempts: None,
                escalated_signature: Some("sig".into()),
            },
        )
        .unwrap();
        assert!(clear_redirect(&db, &issue).unwrap());
        let record = get_supervision(&db, &issue).unwrap().unwrap();
        assert_eq!(record.active_redirect, None);
        assert_eq!(record.escalated_signature.as_deref(), Some("sig"));
        // Idempotent.
        assert!(!clear_redirect(&db, &issue).unwrap());
    }

    #[test]
    fn a_supervision_record_round_trips() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        let record = SupervisionRecord {
            issue: issue.clone(),
            fingerprint: "f".into(),
            progress_observed_at: ago(30),
            first_absent_at: Some(ago(20)),
            last_checked_at: ago(10),
            active_redirect: Some("r".into()),
            redirect_signature: Some("s".into()),
            redirect_issued_after_attempts: None,
            escalated_signature: None,
        };
        upsert_supervision(&db, &record).unwrap();
        assert_eq!(get_supervision(&db, &issue).unwrap().unwrap(), record);
    }

    // ── reconciliation ──────────────────────────────────────────────────

    #[test]
    fn drawer_present_and_alive_is_adopt() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&issue, true)).unwrap();
        let rows = reconcile(&db, &alive_snapshot(&issue)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, ReconcileVerdict::Adopt);
        assert_eq!(rows[0].issue.as_ref(), Some(&issue));
    }

    #[test]
    fn drawer_present_and_gone_is_restart_and_says_whether_a_session_exists() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&issue, false)).unwrap();
        let rows = reconcile(&db, &RegistrySnapshot::Available(Vec::new())).unwrap();
        assert_eq!(
            rows[0].verdict,
            ReconcileVerdict::RestartFromCheckpoint {
                session_claimed: false
            },
            "an unclaimed uuid has no checkpoint to restart from — the next run must open the \
             session, not resume it"
        );
    }

    #[test]
    fn a_live_ic_with_no_drawer_is_an_orphan_and_is_never_adopted() {
        let db = Database::open_in_memory().unwrap();
        let snapshot = RegistrySnapshot::Available(vec![AgentEntry {
            name: "ic-someone-else-99".into(),
            status: None,
        }]);
        let rows = reconcile(&db, &snapshot).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, ReconcileVerdict::Orphan);
        assert_eq!(rows[0].issue, None);
    }

    #[test]
    fn a_non_ic_session_is_not_autopilots_business() {
        let db = Database::open_in_memory().unwrap();
        let snapshot = RegistrySnapshot::Available(vec![AgentEntry {
            name: "jeffs-own-session".into(),
            status: Some("idle".into()),
        }]);
        assert!(reconcile(&db, &snapshot).unwrap().is_empty());
    }

    #[test]
    fn an_unreadable_registry_holds_every_row_and_reports_no_orphans() {
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&issue, true)).unwrap();
        let rows = reconcile(
            &db,
            &RegistrySnapshot::Unavailable {
                reason: "claude not on PATH".into(),
            },
        )
        .unwrap();
        assert_eq!(rows.len(), 1);
        match &rows[0].verdict {
            ReconcileVerdict::Hold { reason } => assert!(reason.contains("claude not on PATH")),
            other => panic!("expected Hold, got {other:?}"),
        }
    }

    #[test]
    fn reconciliation_covers_every_in_flight_issue_at_once() {
        let db = Database::open_in_memory().unwrap();
        let a = IssueRef::new("ironrace/ironmem", 1);
        let b = IssueRef::new("ironrace/other", 2);
        dispatch_state::upsert_dispatch_state(&db, &state_for(&a, true)).unwrap();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&b, true)).unwrap();
        let snapshot = RegistrySnapshot::Available(vec![AgentEntry {
            name: super::super::dispatch::ic_name(&a),
            status: None,
        }]);
        let rows = reconcile(&db, &snapshot).unwrap();
        assert_eq!(rows.len(), 2);
        let for_a = rows.iter().find(|r| r.issue.as_ref() == Some(&a)).unwrap();
        let for_b = rows.iter().find(|r| r.issue.as_ref() == Some(&b)).unwrap();
        assert_eq!(for_a.verdict, ReconcileVerdict::Adopt);
        assert!(matches!(
            for_b.verdict,
            ReconcileVerdict::RestartFromCheckpoint { .. }
        ));
    }

    #[test]
    fn lineage_traffic_cannot_hide_an_in_flight_dispatch_state() {
        // Rung 5's finding #4, in this module's terms: dispatch states share
        // a room with every append-only attempt record, and a reconciliation
        // that could not see a dispatch state would report its live IC as an
        // orphan — a wrong answer, not an error.
        let db = Database::open_in_memory().unwrap();
        let issue = issue();
        dispatch_state::upsert_dispatch_state(&db, &state_for(&issue, true)).unwrap();
        for n in 1..=50u32 {
            lineage::record_attempt(
                &db,
                &attempt(n, AttemptOutcome::Failed, Some(&format!("failure {n}"))),
            )
            .unwrap();
        }
        let rows = reconcile(&db, &alive_snapshot(&issue)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, ReconcileVerdict::Adopt);
    }
}
