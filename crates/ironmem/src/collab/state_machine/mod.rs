use super::agent::Agent;
use super::error::CollabError;
use super::event::CollabEvent;
use super::phase::Phase;
use super::session::CollabSession;
use super::OFF_TURN_FAILURE_PREFIXES;

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
/// After this many reviews, Claude is forced into finalize regardless of the
/// verdict (she always gets the last word).
pub(super) const MAX_REVIEW_ROUNDS: u8 = 2;

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
            next.review_round = session.review_round.saturating_add(1);

            // request_changes returns to synthesis (Claude revises) unless we've
            // hit the cap — then Claude is forced into finalize with the last word.
            let force_finalize = next.review_round >= MAX_REVIEW_ROUNDS;
            if verdict == "request_changes" && !force_finalize {
                next.phase = Phase::PlanSynthesisPending;
                next.current_owner = Agent::Claude;
            } else {
                next.phase = Phase::PlanClaudeFinalizePending;
                next.current_owner = Agent::Claude;
            }
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
            // the user selected at `collab_start` time. Default sessions
            // have `implementer == Agent::Claude` (historical flow);
            // sessions started with `--implementer=codex` route Codex
            // into the batch phase to drive its own
            // subagent-driven-development.
            next.current_owner = session.implementer;
        }
        // ── v3: batch implementation → global review ──────────────────────
        // The implementer agent (Claude by default; Codex when selected at
        // `collab_start`) drives per-task subagent work on its side via
        // `superpowers:writing-plans` → `superpowers:subagent-driven-development`.
        // The other agent does not participate per-task; the single
        // transition out of `CodeImplementPending` jumps straight to
        // global review with Claude as owner — Claude always provides
        // the local-review second opinion. Payload carries only
        // `head_sha` (anti-puppeteering).
        (Phase::CodeImplementPending, CollabEvent::ImplementationDone { head_sha }) => {
            require_actor(actor, session.implementer)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewLocalPending;
            next.current_owner = Agent::Claude;
        }
        // ── v3: global review, 3-phase linear ─────────────────────────────
        (Phase::CodeReviewLocalPending, CollabEvent::ReviewLocal { head_sha }) => {
            require_actor(actor, Agent::Claude)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewFixGlobalPending;
            next.current_owner = Agent::Codex;
        }
        (Phase::CodeReviewFixGlobalPending, CollabEvent::CodeReviewFixGlobal { head_sha }) => {
            require_actor(actor, Agent::Codex)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewFinalPending;
            next.current_owner = Agent::Claude;
        }
        (Phase::CodeReviewFinalPending, CollabEvent::FinalReview { head_sha, pr_url }) => {
            require_actor(actor, Agent::Claude)?;
            next.last_head_sha = Some(head_sha.clone());
            next.pr_url = Some(pr_url.clone());
            next.phase = Phase::CodingComplete;
            next.current_owner = Agent::Claude;
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
            next.coding_failure = Some(coding_failure.clone());
            next.phase = Phase::CodingFailed;
            next.current_owner = actor;
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
