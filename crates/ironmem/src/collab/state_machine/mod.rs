use super::agent::Agent;
use super::error::CollabError;
use super::event::CollabEvent;
use super::phase::Phase;
use super::session::CollabSession;
use super::{
    classify, off_turn_failure_is_admissible, task_count_from_payload, validate_task_list_body,
    FailureClass, TaskListValidationError, MAX_TASKS_PER_COLLAB_ISSUE,
};

/// Construct a fresh `CollabSession` positioned at the v3 global-review
/// stage, for the coding-review shortcut. Rejects empty SHAs so the
/// session never enters the review flow with unset drift-detection state.
/// `pilot` is the agent leading the review session; `new_global_review`
/// derives `current_owner = counterpart(pilot)` and `implementer = pilot`
/// from it. The MCP layer (`handle_collab_start_code_review`) resolves and
/// forwards a real `pilot` choice (defaulting to `Agent::Claude`); this is
/// independent of that same layer's `initiator must be 'claude'` check,
/// which constrains the *dispatcher* invoking the shortcut, not the pilot.
pub fn start_global_review_session(
    id: &str,
    base_sha: &str,
    head_sha: &str,
    pilot: Agent,
) -> Result<CollabSession, CollabError> {
    if base_sha.is_empty() {
        return Err(CollabError::MissingBaseSha);
    }
    if head_sha.is_empty() {
        return Err(CollabError::MissingHeadSha);
    }
    Ok(CollabSession::new_global_review(
        id, base_sha, head_sha, pilot,
    ))
}

/// Maximum number of review cycles the copilot may run on the canonical
/// plan. Planning is intentionally one-pass after the blind drafts: the
/// pilot synthesizes once, the copilot reviews once, then the pilot
/// finalizes the execution-ready task plan.
pub(super) const MAX_REVIEW_ROUNDS: u8 = 1;

/// Maximum number of recoverable ("tooling") `FailureReport`s tolerated per
/// *resume budget* before recovery is abandoned. Two recoverable reports are
/// tolerated (the session stays in recovery, non-terminal); the third — the
/// one whose increment would push `recovery_attempts` past this ceiling —
/// degrades to the terminal `CodingFailed` path instead of recovering
/// again. See the `FailureClass::Tooling` arm of `apply_event` below.
///
/// This ceiling alone does not bound a session: `recovery_attempts` is reset
/// to 0 by a successful delegated completion and by `ResumeCoding`. The
/// lifetime bound is [`MAX_TOTAL_RECOVERY_ATTEMPTS`].
pub const MAX_RECOVERY_ATTEMPTS: u8 = 2;

/// Maximum number of recoverable handoffs tolerated over a session's entire
/// lifetime, counted by the monotonic `total_recovery_attempts`, which
/// nothing resets. Once reached, a further tooling `FailureReport` degrades
/// to `CodingFailed` and `ResumeCoding` is rejected as `NotResumable`.
///
/// Why this exists: `MAX_RECOVERY_ATTEMPTS` bounds a *budget*, not a
/// session. `collab_resume` refreshes that budget, is agent-callable, and is
/// on the unattended successor's permission allowlist — so without a
/// monotonic counter a session could loop failure → ceiling → resume →
/// failure without end, burning tokens, while `collab_status` and the
/// handoff block never showed a count above 2.
///
/// Deliberately NOT a multiple of `MAX_RECOVERY_ATTEMPTS`. At an even value
/// the per-resume ceiling would always trip first (a budget is only ever
/// exhausted at 2, 4, 6 …) and this check would be unreachable. At 5 the
/// lifetime ceiling genuinely binds: after two exhausted budgets (lifetime
/// count 4) and a resume, the next handoff reaches 5 and the one after it
/// degrades the session with the per-resume budget still unspent.
pub const MAX_TOTAL_RECOVERY_ATTEMPTS: u8 = 5;

/// Require an actor to match the expected agent, else return `NotYourTurn`.
fn require_actor(actor: Agent, expected: Agent) -> Result<(), CollabError> {
    if actor == expected {
        Ok(())
    } else {
        Err(CollabError::NotYourTurn {
            expected: expected.to_string(),
            got: actor.to_string(),
        })
    }
}

/// The other agent in the two-party collab protocol. Used to flip
/// `current_owner`/`recovery_owner` to the counterpart of whichever agent
/// reported a recoverable ("tooling") failure.
///
/// `pub(super)` so `session::new_global_review` (a sibling module under
/// `collab`) can derive `current_owner` from a `pilot` argument without
/// reimplementing the same flip logic.
pub(super) fn counterpart(agent: Agent) -> Agent {
    match agent {
        Agent::Claude => Agent::Codex,
        Agent::Codex => Agent::Claude,
    }
}

/// The session's *pilot*: the agent that synthesizes and finalizes the plan
/// (`publish_canonical`, `publish_final`, `submit_task_list`) and audits the
/// copilot's commits (`review_local`, `final_review`). Persisted on the
/// session, so this is a plain read — it exists as a named accessor purely
/// so every role decision in `apply_event` reads as `pilot(session)` /
/// `copilot(session)` rather than a bare field access next to a hardcoded
/// agent literal.
fn pilot(session: &CollabSession) -> Agent {
    session.pilot
}

/// The session's *copilot*: the agent that reviews the canonical plan
/// (`submit_review`) and applies the post-implementation global fixes
/// (`review_fix_global`). Always `counterpart(session.pilot)` — derived on
/// the fly and deliberately never stored, so there is no second column that
/// could drift out of sync with `pilot`.
///
/// Note that `implementer` is an independent knob: it is read straight off
/// the session and is neither derived from nor validated against these two.
///
/// `pub(crate)` (unlike `pilot`, which stays private) so the MCP layer's
/// `collab_approve` handler can gate on the same derivation this module
/// enforces with, rather than re-deriving the copilot from `session.pilot`
/// itself and creating a second place that could drift.
pub(crate) fn copilot(session: &CollabSession) -> Agent {
    counterpart(session.pilot)
}

/// Like `require_actor`, but additionally permits a one-turn delegated
/// completion: if `actor` is the agent recovery handed control to
/// (`session.recovery_owner == Some(actor)`) AND the session is still in the
/// exact phase that was interrupted (`session.recovery_phase ==
/// Some(session.phase)`), the actor may complete the coding-phase event in
/// place of `expected`. Otherwise this delegates to `require_actor`
/// verbatim, so behavior is unchanged when there's no active recovery.
///
/// This function only decides whether the call is allowed — it does not
/// clear any recovery state. Callers are responsible for clearing
/// `pending_failure`/`recovery_phase`/`recovery_owner`/
/// `recovery_origin_owner`/`recovery_attempts` in `next` on acceptance.
fn require_actor_or_recovery(
    session: &CollabSession,
    actor: Agent,
    expected: Agent,
) -> Result<(), CollabError> {
    if session.recovery_owner == Some(actor) && session.recovery_phase == Some(session.phase) {
        return Ok(());
    }
    require_actor(actor, expected)
}

/// Clear all recovery bookkeeping on a successful coding-phase completion
/// event. Applied unconditionally by all four `require_actor_or_recovery`
/// call sites, not just when the override fired: a session with no active
/// recovery already has these fields at their zero values
/// (`None`/`None`/`None`/`None`/`0`), so this is a no-op in the normal case
/// and the required clear in the recovery case — simpler than conditioning
/// on whether the override actually fired, and provably correct either way.
///
/// `total_recovery_attempts` is deliberately NOT cleared here. It is the
/// lifetime counter behind [`MAX_TOTAL_RECOVERY_ATTEMPTS`]; zeroing it on a
/// success would let a session that alternates failure/recovery/failure run
/// unbounded, which is the exact hole the counter exists to close.
fn clear_recovery_state(next: &mut CollabSession) {
    next.pending_failure = None;
    next.recovery_phase = None;
    next.recovery_owner = None;
    next.recovery_origin_owner = None;
    next.recovery_attempts = 0;
}

/// Clear only the three recovery *pointer* fields (`recovery_phase`,
/// `recovery_owner`, `recovery_origin_owner`), leaving `pending_failure` and
/// `recovery_attempts` to the caller. Used by both terminal paths in the
/// `FailureReport` arm — the retry-ceiling degrade and the direct
/// `FailureClass::Terminal` branch — so a `CodingFailed` session never
/// carries stale recovery pointers alongside a real `coding_failure`. Unlike
/// [`clear_recovery_state`] (reserved for *successful* delegated completion,
/// which also resets `recovery_attempts` to 0), a terminal report is not a
/// success: `recovery_attempts` is left for the caller to decide, and
/// `pending_failure` is cleared explicitly at each call site rather than
/// here, to keep this helper's effect obvious from its name.
fn clear_recovery_pointers(next: &mut CollabSession) {
    next.recovery_phase = None;
    next.recovery_owner = None;
    next.recovery_origin_owner = None;
}

/// Determine whether a `CodingFailed` session is eligible for `ResumeCoding`,
/// returning the `Phase` to restore on success. Admission requires ALL of
/// `failed_from_phase.is_some()`, the stored `coding_failure` classifying as
/// `FailureClass::Tooling`, and `total_recovery_attempts` still being below
/// [`MAX_TOTAL_RECOVERY_ATTEMPTS`] — a session that fails any check returns
/// a specific `NotResumable` reason rather than falling through to the
/// generic `SessionLocked`.
///
/// The lifetime-ceiling check is what makes the ceiling a ceiling. Without
/// it, resume refreshes `recovery_attempts` and the session can fail its way
/// to terminal and back indefinitely.
///
/// The rejection messages are deliberately distinct: a `None`
/// `failed_from_phase` means the row predates migration 015/Task 4 (a fact
/// about schema provenance, stated plainly — never guessed at); a `Some`
/// `failed_from_phase` whose `coding_failure` classifies `Terminal` (e.g.
/// `branch_drift:`, `subagent_failure:`) means the session failed for a
/// reason that was never recoverable in the first place.
///
/// Pure and side-effect-free — safe to call more than once against the same
/// immutable `session` (the top-of-function guard and the `ResumeCoding`
/// match arm both call it independently rather than threading a value
/// through).
fn resume_eligibility(session: &CollabSession) -> Result<Phase, CollabError> {
    let Some(restored_phase) = session.failed_from_phase else {
        return Err(CollabError::NotResumable {
            reason: "session predates resume support: failed_from_phase was never recorded \
                     for this CodingFailed row (pre-migration-015)"
                .to_string(),
        });
    };
    let is_tooling = session
        .coding_failure
        .as_deref()
        .is_some_and(|failure| classify(failure) == FailureClass::Tooling);
    if !is_tooling {
        return Err(CollabError::NotResumable {
            reason: "coding_failure does not classify as a recoverable tooling failure".to_string(),
        });
    }
    if session.total_recovery_attempts >= MAX_TOTAL_RECOVERY_ATTEMPTS {
        return Err(CollabError::NotResumable {
            reason: format!(
                "lifetime recovery ceiling reached: {} handoffs across this session's lifetime \
                 (max {MAX_TOTAL_RECOVERY_ATTEMPTS}); resuming again would refresh the per-resume \
                 budget without bound",
                session.total_recovery_attempts
            ),
        });
    }
    Ok(restored_phase)
}

pub fn apply_event(
    session: &CollabSession,
    actor: Agent,
    event: &CollabEvent,
) -> Result<CollabSession, CollabError> {
    // v3: terminal coding phases reject all further events, with exactly one
    // carve-out: `ResumeCoding` from `CodingFailed`. `CodingComplete` gets
    // zero carve-outs — any event from it, including `ResumeCoding`, hits
    // `SessionLocked` below. An ineligible `ResumeCoding` from `CodingFailed`
    // (semantic failure, or a legacy row) returns the specific
    // `NotResumable` reason right here — NOT `SessionLocked`, and NOT a
    // fall-through to the `WrongPhase` catch-all — so callers can tell "this
    // session is locked" apart from "this session is locked AND could never
    // have resumed anyway".
    if matches!(session.phase, Phase::CodingComplete | Phase::CodingFailed) {
        let is_resume_attempt =
            session.phase == Phase::CodingFailed && matches!(event, CollabEvent::ResumeCoding);
        if !is_resume_attempt {
            return Err(CollabError::SessionLocked);
        }
        resume_eligibility(session)?;
    }

    let mut next = session.clone();

    match (&session.phase, event) {
        (Phase::PlanParallelDrafts, CollabEvent::SubmitDraft { content_hash }) => {
            // The two draft-hash columns are keyed by agent *identity*, not
            // by role — `claude_draft_hash` holds Claude's draft under any
            // pilot assignment — so slot selection stays a literal
            // `actor` / `counterpart(actor)` split. That axis is independent
            // of the pilot/copilot decision made further down.
            let (own_slot, other_has_drafted) = match actor {
                Agent::Claude => {
                    let other = next.codex_draft_hash.is_some();
                    (&mut next.claude_draft_hash, other)
                }
                Agent::Codex => {
                    let other = next.claude_draft_hash.is_some();
                    (&mut next.codex_draft_hash, other)
                }
            };
            if own_slot.is_some() {
                return Err(CollabError::AlreadySubmittedDraft {
                    agent: actor.to_string(),
                });
            }
            *own_slot = Some(content_hash.clone());
            if other_has_drafted {
                // Both blind drafts are in. Synthesis is the pilot's job,
                // whichever agent happened to submit second.
                next.phase = Phase::PlanSynthesisPending;
                next.current_owner = pilot(session);
            } else {
                // Exactly one draft is in and it is `actor`'s, so the agent
                // still owing a draft *is* `counterpart(actor)` — hand it
                // the turn. This one branch reproduces both of the old
                // per-agent arms: under `pilot=claude`, an `actor=Claude`
                // first draft yields Codex (as before), and an
                // `actor=Codex` first draft yields Claude — which the old
                // Codex arm reached by unconditionally assigning Claude.
                next.current_owner = counterpart(actor);
            }
        }
        (Phase::PlanSynthesisPending, CollabEvent::PublishCanonical { content_hash }) => {
            require_actor(actor, pilot(session))?;
            next.canonical_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanCopilotReviewPending;
            next.current_owner = copilot(session);
        }
        (Phase::PlanCopilotReviewPending, CollabEvent::SubmitReview { verdict }) => {
            require_actor(actor, copilot(session))?;
            if !matches!(
                verdict.as_str(),
                "approve" | "approve_with_minor_edits" | "request_changes"
            ) {
                return Err(CollabError::InvalidVerdictValue(verdict.clone()));
            }
            next.codex_review_verdict = Some(verdict.clone());
            // The `.min(MAX_REVIEW_ROUNDS)` clamp is defensive-only today: with a
            // single one-pass review the phase never re-enters synthesis, so a
            // second `SubmitReview` can't fire and the bump can't exceed the cap. It
            // becomes load-bearing the moment a synthesis loop is re-added — keep it.
            next.review_round = session
                .review_round
                .saturating_add(1)
                .min(MAX_REVIEW_ROUNDS);

            // The copilot gets exactly one review pass. Any requested
            // changes are folded into the pilot's final execution-ready task
            // plan; planning never re-enters synthesis.
            next.phase = Phase::PlanFinalizePending;
            next.current_owner = pilot(session);
        }
        (Phase::PlanFinalizePending, CollabEvent::PublishFinal { content_hash }) => {
            require_actor(actor, pilot(session))?;
            next.final_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanLocked;
        }
        // ── v3: the one transition out of PlanLocked ──────────────────────
        (
            Phase::PlanLocked,
            CollabEvent::SubmitTaskList {
                plan_hash,
                base_sha,
                task_list_json,
                tasks_count,
                head_sha,
            },
        ) => {
            require_actor(actor, pilot(session))?;
            let expected = session
                .final_plan_hash
                .as_deref()
                .ok_or(CollabError::PlanNotFinalized)?;
            if plan_hash != expected {
                return Err(CollabError::PlanHashMismatch {
                    expected: expected.to_string(),
                    got: plan_hash.clone(),
                });
            }
            let payload =
                serde_json::from_str(task_list_json).map_err(|_| CollabError::InvalidTaskList)?;
            let parsed_tasks_count = match task_count_from_payload(&payload) {
                Ok(count) => count,
                Err(TaskListValidationError::EmptyTasks) => {
                    return Err(CollabError::EmptyTaskList);
                }
                Err(TaskListValidationError::Invalid(_)) => {
                    return Err(CollabError::InvalidTaskList);
                }
                Err(TaskListValidationError::TooManyTasks { actual }) => {
                    return Err(CollabError::TooManyTasks {
                        actual,
                        max: MAX_TASKS_PER_COLLAB_ISSUE,
                    });
                }
            };
            if parsed_tasks_count != *tasks_count {
                return Err(CollabError::TaskListCountMismatch {
                    declared: *tasks_count,
                    actual: parsed_tasks_count,
                });
            }
            match validate_task_list_body(&payload) {
                Ok(_) => {}
                Err(TaskListValidationError::EmptyTasks) => {
                    return Err(CollabError::EmptyTaskList);
                }
                Err(TaskListValidationError::TooManyTasks { actual }) => {
                    return Err(CollabError::TooManyTasks {
                        actual,
                        max: MAX_TASKS_PER_COLLAB_ISSUE,
                    });
                }
                Err(TaskListValidationError::Invalid(_)) => {
                    return Err(CollabError::InvalidTaskList);
                }
            }
            if base_sha.is_empty() {
                return Err(CollabError::MissingBaseSha);
            }
            next.task_list = Some(task_list_json.clone());
            next.task_review_round = 0;
            next.global_review_round = 0;
            next.base_sha = Some(base_sha.clone());
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeImplementPending;
            // Owner of the batch implementation phase is whichever agent
            // the user selected most recently. Default sessions have
            // `implementer == Agent::Claude` (historical flow); sessions
            // started or joined with `--implementer=codex` route Codex into
            // the batch phase to drive its own iron-build.
            next.current_owner = session.implementer;
        }
        // ── v3: batch implementation → global review ──────────────────────
        // The implementer drives per-task subagent work on its side via
        // `iron-build` (or directly, per `execution_mode` — the server never
        // observes which); the other agent does not participate per-task. The
        // single transition out of `CodeImplementPending` jumps to global
        // review with the copilot as owner — copilot first; the pilot audits
        // after. Payload carries only `head_sha` (anti-puppeteering).
        //
        // The actor check stays keyed to `session.implementer`: who wrote the
        // code is a knob orthogonal to the pilot/copilot role split, so a
        // session may legitimately have its implementer equal to either role.
        (Phase::CodeImplementPending, CollabEvent::ImplementationDone { head_sha }) => {
            require_actor_or_recovery(session, actor, session.implementer)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewFixGlobalPending;
            next.current_owner = copilot(session);
            clear_recovery_state(&mut next);
        }
        // ── v3: global review, 3-phase linear (copilot first; pilot audits after) ──
        (Phase::CodeReviewFixGlobalPending, CollabEvent::CodeReviewFixGlobal { head_sha }) => {
            require_actor_or_recovery(session, actor, copilot(session))?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewLocalPending;
            next.current_owner = pilot(session);
            clear_recovery_state(&mut next);
        }
        (Phase::CodeReviewLocalPending, CollabEvent::ReviewLocal { head_sha }) => {
            require_actor_or_recovery(session, actor, pilot(session))?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewFinalPending;
            next.current_owner = pilot(session);
            clear_recovery_state(&mut next);
        }
        (Phase::CodeReviewFinalPending, CollabEvent::FinalReview { head_sha, pr_url }) => {
            require_actor_or_recovery(session, actor, pilot(session))?;
            next.last_head_sha = Some(head_sha.clone());
            next.pr_url = Some(pr_url.clone());
            next.phase = Phase::CodingComplete;
            next.current_owner = pilot(session);
            clear_recovery_state(&mut next);
        }
        // ── v3: failure is valid from any coding-active phase ─────────────
        (phase, CollabEvent::FailureReport { coding_failure }) if phase.is_coding_active() => {
            // Some failure classes are structurally detectable only from
            // outside the owner's process (branch drift via git ops; a
            // Codex dispatch failure observed from Claude's MCP call when
            // `--implementer=codex` and Codex itself never returned). For
            // those, allow the non-owner to emit a `FailureReport` with a
            // recognized prefix; everything else still requires the
            // current owner.
            //
            // The carve-out additionally requires *content* after the
            // prefix: a bare prefix string would let any authenticated
            // session participant abort the session with no diagnostic
            // value, so we reject the empty form and demand at least one
            // byte of context.
            let is_off_turn_admissible =
                off_turn_failure_is_admissible(coding_failure, actor, session.current_owner);
            if !is_off_turn_admissible && actor != session.current_owner {
                return Err(CollabError::NotYourTurn {
                    expected: session.current_owner.to_string(),
                    got: actor.to_string(),
                });
            }
            match classify(coding_failure) {
                // Recoverable tooling failure: stay in the current phase,
                // hand control to the counterpart agent to drive recovery,
                // and record recovery state. `coding_failure` is left
                // untouched (it is always `None` here — `CodingFailed` is
                // terminal and unreachable once entered, so no in-flight
                // coding-active session can already have it set) *unless*
                // the retry ceiling is exceeded, in which case this report
                // degrades to the terminal path below instead.
                FailureClass::Tooling => {
                    // Check what the NEW attempt count would be, not the
                    // current one: two recoverable reports are tolerated
                    // (attempts 0→1, 1→2 both recover), but the third
                    // report — whose increment would take attempts to 3,
                    // exceeding `MAX_RECOVERY_ATTEMPTS` — abandons recovery
                    // instead of incrementing again.
                    //
                    // Both ceilings are checked, and either one degrades the
                    // report to terminal. The per-resume budget bounds one
                    // stretch of recovery; the monotonic lifetime counter
                    // bounds the session, since `ResumeCoding` refreshes the
                    // former but never the latter.
                    let budget_exhausted =
                        session.recovery_attempts.saturating_add(1) > MAX_RECOVERY_ATTEMPTS;
                    let lifetime_exhausted = session.total_recovery_attempts.saturating_add(1)
                        > MAX_TOTAL_RECOVERY_ATTEMPTS;
                    if budget_exhausted || lifetime_exhausted {
                        // Degrade to terminal, mirroring `FailureClass::Terminal`
                        // below but using THIS report's own diagnostic (the
                        // one that broke the ceiling), not an earlier
                        // attempt's. `recovery_attempts` is deliberately left
                        // as-is (it is not reset to 0 nor bumped to 3):
                        // `clear_recovery_state`'s zeroing is reserved for
                        // *successful* delegated completion, which this is
                        // not. `recovery_phase`/`recovery_owner`/
                        // `recovery_origin_owner` are cleared so a
                        // `CodingFailed` session doesn't carry stale
                        // recovery pointers alongside a real
                        // `coding_failure`.
                        next.coding_failure = Some(coding_failure.clone());
                        next.failed_from_phase = Some(*phase);
                        next.pending_failure = None;
                        clear_recovery_pointers(&mut next);
                        next.phase = Phase::CodingFailed;
                        next.current_owner = actor;
                    } else {
                        // Most tooling reports come from the interrupted
                        // turn's owner, so this normally equals
                        // `counterpart(actor)`. `codex_dispatch_failed:` is
                        // deliberately off-turn-admissible, though: Claude
                        // can report an unavailable Codex turn. Derive the
                        // recovery owner from the interrupted owner rather
                        // than the observing reporter so that case hands the
                        // work to Claude instead of back to unavailable
                        // Codex.
                        let interrupted_owner = session.current_owner;
                        let owner = counterpart(interrupted_owner);
                        next.pending_failure = Some(coding_failure.clone());
                        next.recovery_phase = Some(*phase);
                        next.recovery_owner = Some(owner);
                        next.recovery_origin_owner = Some(interrupted_owner);
                        next.recovery_attempts = session.recovery_attempts.saturating_add(1);
                        next.total_recovery_attempts =
                            session.total_recovery_attempts.saturating_add(1);
                        next.current_owner = owner;
                    }
                }
                // Terminal (unrecoverable) failure: today's exact behavior,
                // plus capturing the phase the session was in at the time
                // of failure and defensively clearing `pending_failure`.
                // `clear_recovery_pointers` also clears the three recovery
                // pointer fields: a Terminal report can arrive while a prior
                // Tooling recovery is still in flight (same phase, not yet
                // resolved by a delegated completion), and without this the
                // resulting `CodingFailed` session would carry stale
                // `recovery_owner`/`recovery_phase`/`recovery_origin_owner`
                // alongside a real `coding_failure`. `recovery_attempts` is
                // deliberately left as-is, same as the retry-ceiling degrade
                // path above — it's diagnostic history, not live state, once
                // the session is terminal.
                FailureClass::Terminal => {
                    next.coding_failure = Some(coding_failure.clone());
                    next.failed_from_phase = Some(*phase);
                    next.pending_failure = None;
                    clear_recovery_pointers(&mut next);
                    next.phase = Phase::CodingFailed;
                    next.current_owner = actor;
                }
            }
        }
        // ── ResumeCoding: the one carve-out through the terminal guard ────
        (Phase::CodingFailed, CollabEvent::ResumeCoding) => {
            // Eligibility (`failed_from_phase.is_some()` AND `classify(...)
            // == Tooling`) was already enforced by the top-of-function
            // guard — an ineligible resume attempt returns `NotResumable`
            // there and never reaches this arm. Recomputing here is pure
            // and side-effect-free, so this can only fail if the guard and
            // this call somehow disagreed on the same immutable `session`,
            // which they cannot.
            let restored_phase = resume_eligibility(session)?;
            next.phase = restored_phase;
            // "The resuming counterpart" = whichever agent is calling
            // resume (`actor`), not `counterpart(actor)`: the resumer
            // becomes both the new current owner and its own recorded
            // recovery owner.
            next.current_owner = actor;
            next.recovery_owner = Some(actor);
            // Mirrors a freshly-recovered Tooling session so a subsequent
            // `require_actor_or_recovery` call at `restored_phase` accepts
            // this same resumer via the existing Task 5 mechanism, without
            // a special case.
            next.recovery_phase = Some(restored_phase);
            // Move the terminal diagnostic into `pending_failure` for
            // audit, mirroring the "ResumeCoding accepted" row of the
            // binding-design-decisions table in the plan doc.
            next.pending_failure = session.coding_failure.clone();
            next.coding_failure = None;
            // `failed_from_phase` is deliberately left set: it is a
            // historical record of which phase this session originally
            // failed from, which stays useful for audit even after resume
            // and does not conflict with any acceptance criterion.
            //
            // `recovery_origin_owner` is already `None` here — Task 6's
            // `clear_recovery_pointers` clears it on every transition into
            // `CodingFailed` (both the ceiling degrade and the direct
            // Terminal branch), so there is nothing stale to clear.
            //
            // Resume begins a fresh recovery *budget*. The terminal row's
            // exhausted count is historical diagnostic state; retaining it
            // here would make the first new tooling failure immediately
            // re-hit the retry ceiling instead of handing off recovery.
            //
            // `total_recovery_attempts` is emphatically not reset alongside
            // it — that is the whole reason the field exists. Resume is
            // already refused above once the lifetime ceiling is reached, so
            // the refreshed budget is always bounded by whatever lifetime
            // headroom is left.
            next.recovery_attempts = 0;
        }
        (phase, _) => {
            // Terminal phases are short-circuited by the guard at the top of
            // this function, so they never reach here. The debug_assert
            // catches any future refactor that reorders or removes the guard.
            debug_assert!(
                !matches!(phase, Phase::CodingComplete | Phase::CodingFailed),
                "terminal phase {phase:?} reached WrongPhase catch-all",
            );
            return Err(CollabError::WrongPhase {
                expected: phase.expected_event().to_string(),
                got: event.name().to_string(),
            });
        }
    }

    Ok(next)
}

#[cfg(test)]
mod tests;
