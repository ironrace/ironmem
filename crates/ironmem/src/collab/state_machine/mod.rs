use super::agent::Agent;
use super::error::CollabError;
use super::event::CollabEvent;
use super::phase::Phase;
use super::session::CollabSession;
use super::{classify, FailureClass, OFF_TURN_FAILURE_PREFIXES};

/// Construct a fresh `CollabSession` positioned at the v3 global-review
/// stage, for the coding-review shortcut. Rejects empty SHAs so the
/// session never enters the review flow with unset drift-detection state.
pub fn start_global_review_session(
    id: &str,
    base_sha: &str,
    head_sha: &str,
) -> Result<CollabSession, CollabError> {
    if base_sha.is_empty() {
        return Err(CollabError::MissingBaseSha);
    }
    if head_sha.is_empty() {
        return Err(CollabError::MissingHeadSha);
    }
    Ok(CollabSession::new_global_review(id, base_sha, head_sha))
}

/// Maximum number of review cycles Codex may run on the canonical plan.
/// Planning is intentionally one-pass after the blind drafts: Claude
/// synthesizes once, Codex reviews once, then Claude finalizes the
/// execution-ready task plan.
pub(super) const MAX_REVIEW_ROUNDS: u8 = 1;

/// Maximum number of recoverable ("tooling") `FailureReport`s tolerated per
/// session before recovery is abandoned. Two recoverable reports are
/// tolerated (the session stays in recovery, non-terminal); the third — the
/// one whose increment would push `recovery_attempts` past this ceiling —
/// degrades to the terminal `CodingFailed` path instead of recovering
/// again. See the `FailureClass::Tooling` arm of `apply_event` below.
pub const MAX_RECOVERY_ATTEMPTS: u8 = 2;

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
fn counterpart(agent: Agent) -> Agent {
    match agent {
        Agent::Claude => Agent::Codex,
        Agent::Codex => Agent::Claude,
    }
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
fn clear_recovery_state(next: &mut CollabSession) {
    next.pending_failure = None;
    next.recovery_phase = None;
    next.recovery_owner = None;
    next.recovery_origin_owner = None;
    next.recovery_attempts = 0;
}

pub fn apply_event(
    session: &CollabSession,
    actor: Agent,
    event: &CollabEvent,
) -> Result<CollabSession, CollabError> {
    // v3: terminal coding phases reject all further events. PlanLocked is
    // transient pre-`task_list`; the only transition out of it is a
    // `SubmitTaskList` from Claude.
    if matches!(session.phase, Phase::CodingComplete | Phase::CodingFailed) {
        return Err(CollabError::SessionLocked);
    }

    let mut next = session.clone();

    match (&session.phase, event) {
        (Phase::PlanParallelDrafts, CollabEvent::SubmitDraft { content_hash }) => match actor {
            Agent::Claude => {
                if session.claude_draft_hash.is_some() {
                    return Err(CollabError::AlreadySubmittedDraft {
                        agent: actor.to_string(),
                    });
                }
                next.claude_draft_hash = Some(content_hash.clone());
                if session.codex_draft_hash.is_some() {
                    next.phase = Phase::PlanSynthesisPending;
                    next.current_owner = Agent::Claude;
                } else {
                    next.current_owner = Agent::Codex;
                }
            }
            Agent::Codex => {
                if session.codex_draft_hash.is_some() {
                    return Err(CollabError::AlreadySubmittedDraft {
                        agent: actor.to_string(),
                    });
                }
                next.codex_draft_hash = Some(content_hash.clone());
                // Whether Claude has drafted or not, the next owner is
                // always Claude — either to synthesize or to wait for
                // Codex's draft to land first.
                next.current_owner = Agent::Claude;
                if session.claude_draft_hash.is_some() {
                    next.phase = Phase::PlanSynthesisPending;
                }
            }
        },
        (Phase::PlanSynthesisPending, CollabEvent::PublishCanonical { content_hash }) => {
            require_actor(actor, Agent::Claude)?;
            next.canonical_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanCodexReviewPending;
            next.current_owner = Agent::Codex;
        }
        (Phase::PlanCodexReviewPending, CollabEvent::SubmitReview { verdict }) => {
            require_actor(actor, Agent::Codex)?;
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

            // Codex gets exactly one review pass. Any requested changes are
            // folded into Claude's final execution-ready task plan; planning
            // never re-enters synthesis.
            next.phase = Phase::PlanClaudeFinalizePending;
            next.current_owner = Agent::Claude;
        }
        (Phase::PlanClaudeFinalizePending, CollabEvent::PublishFinal { content_hash }) => {
            require_actor(actor, Agent::Claude)?;
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
            require_actor(actor, Agent::Claude)?;
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
            if *tasks_count == 0 {
                return Err(CollabError::EmptyTaskList);
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
            // the batch phase to drive its own subagent-driven-development.
            next.current_owner = session.implementer;
        }
        // ── v3: batch implementation → global review ──────────────────────
        // The implementer agent (Claude by default; Codex when selected at
        // `collab_start`) drives per-task subagent work on its side via
        // `superpowers:writing-plans` → `superpowers:subagent-driven-development`.
        // The other agent does not participate per-task; the single
        // transition out of `CodeImplementPending` jumps to global review
        // with Codex as owner — Codex first; Claude audits after. Payload
        // carries only `head_sha` (anti-puppeteering).
        (Phase::CodeImplementPending, CollabEvent::ImplementationDone { head_sha }) => {
            require_actor_or_recovery(session, actor, session.implementer)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewFixGlobalPending;
            next.current_owner = Agent::Codex;
            clear_recovery_state(&mut next);
        }
        // ── v3: global review, 3-phase linear (Codex first; Claude audits after) ──
        (Phase::CodeReviewFixGlobalPending, CollabEvent::CodeReviewFixGlobal { head_sha }) => {
            require_actor_or_recovery(session, actor, Agent::Codex)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewLocalPending;
            next.current_owner = Agent::Claude;
            clear_recovery_state(&mut next);
        }
        (Phase::CodeReviewLocalPending, CollabEvent::ReviewLocal { head_sha }) => {
            require_actor_or_recovery(session, actor, Agent::Claude)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewFinalPending;
            next.current_owner = Agent::Claude;
            clear_recovery_state(&mut next);
        }
        (Phase::CodeReviewFinalPending, CollabEvent::FinalReview { head_sha, pr_url }) => {
            require_actor_or_recovery(session, actor, Agent::Claude)?;
            next.last_head_sha = Some(head_sha.clone());
            next.pr_url = Some(pr_url.clone());
            next.phase = Phase::CodingComplete;
            next.current_owner = Agent::Claude;
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
            let is_off_turn_admissible = OFF_TURN_FAILURE_PREFIXES.iter().any(|prefix| {
                coding_failure.starts_with(prefix) && coding_failure.len() > prefix.len()
            });
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
                    if session.recovery_attempts.saturating_add(1) > MAX_RECOVERY_ATTEMPTS {
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
                        next.recovery_phase = None;
                        next.recovery_owner = None;
                        next.recovery_origin_owner = None;
                        next.phase = Phase::CodingFailed;
                        next.current_owner = actor;
                    } else {
                        let owner = counterpart(actor);
                        next.pending_failure = Some(coding_failure.clone());
                        next.recovery_phase = Some(*phase);
                        next.recovery_owner = Some(owner);
                        next.recovery_origin_owner = Some(actor);
                        next.recovery_attempts = session.recovery_attempts.saturating_add(1);
                        next.current_owner = owner;
                    }
                }
                // Terminal (unrecoverable) failure: today's exact behavior,
                // plus capturing the phase the session was in at the time
                // of failure and defensively clearing `pending_failure`.
                FailureClass::Terminal => {
                    next.coding_failure = Some(coding_failure.clone());
                    next.failed_from_phase = Some(*phase);
                    next.pending_failure = None;
                    next.phase = Phase::CodingFailed;
                    next.current_owner = actor;
                }
            }
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
