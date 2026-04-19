use super::error::CollabError;
use super::event::CollabEvent;
use super::phase::Phase;
use super::session::CollabSession;
use super::BRANCH_DRIFT_PREFIX;

/// Maximum number of review cycles Codex may run on the canonical plan.
/// After this many reviews, Claude is forced into finalize regardless of the
/// verdict (she always gets the last word).
pub(super) const MAX_REVIEW_ROUNDS: u8 = 2;

/// Maximum number of Codex-review debate rounds per coding task. At the cap,
/// Claude's `verdict=disagree_with_reasons` skips Debate and lands directly
/// in `CodeFinalPending`, which advances the task instead of looping back.
pub(super) const MAX_TASK_REVIEW_ROUNDS: u8 = 2;

/// Maximum number of Codex disagree rounds during global review. At the cap,
/// `CodeReviewFinalPending` advances straight to `PrReadyPending` instead of
/// looping back for another Codex pass.
pub(super) const MAX_GLOBAL_REVIEW_ROUNDS: u8 = 2;

/// The set of verdicts accepted on v2 coding topics (`verdict`,
/// `verdict_global`, `review_global`). `review_global` uses the same strings
/// even though only Codex sends it — keeping the vocabulary uniform means
/// harness code can share a verdict-parsing helper.
pub(super) const CODING_VERDICTS: [&str; 2] = ["agree", "disagree_with_reasons"];

/// Require an actor to match the expected value, else return `NotYourTurn`.
fn require_actor(actor: &str, expected: &str) -> Result<(), CollabError> {
    if actor == expected {
        Ok(())
    } else {
        Err(CollabError::NotYourTurn {
            expected: expected.to_string(),
            got: actor.to_string(),
        })
    }
}

/// Validate one of the coding-loop verdict strings.
fn validate_coding_verdict(verdict: &str) -> Result<(), CollabError> {
    if CODING_VERDICTS.contains(&verdict) {
        Ok(())
    } else {
        Err(CollabError::InvalidVerdictValue(verdict.to_string()))
    }
}

pub fn apply_event(
    session: &CollabSession,
    actor: &str,
    event: &CollabEvent,
) -> Result<CollabSession, CollabError> {
    // v2: PlanLocked is transient pre-`task_list`. The ONLY transition out of
    // it is a `SubmitTaskList` from Claude — anything else is rejected as
    // SessionLocked. The terminal coding phases reject all further events.
    if matches!(session.phase, Phase::CodingComplete | Phase::CodingFailed) {
        return Err(CollabError::SessionLocked);
    }

    let mut next = session.clone();

    match (&session.phase, event) {
        (Phase::PlanParallelDrafts, CollabEvent::SubmitDraft { content_hash }) => match actor {
            "claude" => {
                if session.claude_draft_hash.is_some() {
                    return Err(CollabError::AlreadySubmittedDraft {
                        agent: actor.to_string(),
                    });
                }
                next.claude_draft_hash = Some(content_hash.clone());
                if session.codex_draft_hash.is_some() {
                    next.phase = Phase::PlanSynthesisPending;
                    next.current_owner = "claude".to_string();
                } else {
                    next.current_owner = "codex".to_string();
                }
            }
            "codex" => {
                if session.codex_draft_hash.is_some() {
                    return Err(CollabError::AlreadySubmittedDraft {
                        agent: actor.to_string(),
                    });
                }
                next.codex_draft_hash = Some(content_hash.clone());
                if session.claude_draft_hash.is_some() {
                    next.phase = Phase::PlanSynthesisPending;
                    next.current_owner = "claude".to_string();
                } else {
                    next.current_owner = "claude".to_string();
                }
            }
            _ => {
                return Err(CollabError::NotYourTurn {
                    expected: "claude|codex".to_string(),
                    got: actor.to_string(),
                });
            }
        },
        (Phase::PlanSynthesisPending, CollabEvent::PublishCanonical { content_hash }) => {
            require_actor(actor, "claude")?;
            next.canonical_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanCodexReviewPending;
            next.current_owner = "codex".to_string();
        }
        (Phase::PlanCodexReviewPending, CollabEvent::SubmitReview { verdict }) => {
            require_actor(actor, "codex")?;
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
                next.current_owner = "claude".to_string();
            } else {
                next.phase = Phase::PlanClaudeFinalizePending;
                next.current_owner = "claude".to_string();
            }
        }
        (Phase::PlanClaudeFinalizePending, CollabEvent::PublishFinal { content_hash }) => {
            require_actor(actor, "claude")?;
            next.final_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanLocked;
        }
        // ── v2: the one transition out of PlanLocked ──────────────────────
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
            require_actor(actor, "claude")?;
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
            next.current_task_index = Some(0);
            next.task_review_round = 0;
            next.global_review_round = 0;
            next.base_sha = Some(base_sha.clone());
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeImplementPending;
            next.current_owner = "claude".to_string();
        }
        // ── v2: per-task 5-phase debate ───────────────────────────────────
        (Phase::CodeImplementPending, CollabEvent::CodeImplement { head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewPending;
            next.current_owner = "codex".to_string();
        }
        (Phase::CodeReviewPending, CollabEvent::CodeReview { head_sha }) => {
            require_actor(actor, "codex")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeVerdictPending;
            next.current_owner = "claude".to_string();
        }
        (Phase::CodeVerdictPending, CollabEvent::CodeVerdict { verdict, head_sha }) => {
            require_actor(actor, "claude")?;
            validate_coding_verdict(verdict)?;
            next.last_head_sha = Some(head_sha.clone());
            if verdict == "agree" {
                next.advance_task();
            } else {
                // disagree_with_reasons: bump the debate counter. At cap, skip
                // the Debate phase and go straight to Final — Claude still has
                // the last word but Codex gets no further rebuttal.
                next.task_review_round = session.task_review_round.saturating_add(1);
                if next.task_review_round >= MAX_TASK_REVIEW_ROUNDS {
                    next.phase = Phase::CodeFinalPending;
                    next.current_owner = "claude".to_string();
                } else {
                    next.phase = Phase::CodeDebatePending;
                    next.current_owner = "codex".to_string();
                }
            }
        }
        (Phase::CodeDebatePending, CollabEvent::CodeComment { head_sha }) => {
            require_actor(actor, "codex")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeFinalPending;
            next.current_owner = "claude".to_string();
        }
        (Phase::CodeFinalPending, CollabEvent::CodeFinal { head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            // Read from `next` so the check is robust if a future refactor
            // mutates `next.task_review_round` in this arm. `next` is a fresh
            // clone of `session`, so values are equal today.
            if next.task_review_round >= MAX_TASK_REVIEW_ROUNDS {
                // Round cap reached — force advance instead of looping back.
                next.advance_task();
            } else {
                // Under the cap: loop back so Codex re-reviews Claude's fixes.
                // `task_review_round` is preserved across the loopback so the
                // next CodeVerdictPending→CodeFinalPending cycle sees the
                // incremented counter.
                next.phase = Phase::CodeReviewPending;
                next.current_owner = "codex".to_string();
            }
        }
        // ── v2: local review (Claude solo) ────────────────────────────────
        (Phase::CodeReviewLocalPending, CollabEvent::ReviewLocal { head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewCodexPending;
            next.current_owner = "codex".to_string();
        }
        // ── v2: global Codex review (4-phase, 2-pass) ─────────────────────
        (Phase::CodeReviewCodexPending, CollabEvent::ReviewGlobal { verdict, head_sha }) => {
            require_actor(actor, "codex")?;
            validate_coding_verdict(verdict)?;
            next.last_head_sha = Some(head_sha.clone());
            if verdict == "agree" {
                next.phase = Phase::PrReadyPending;
                next.current_owner = "claude".to_string();
            } else {
                next.global_review_round = session.global_review_round.saturating_add(1);
                next.phase = Phase::CodeReviewVerdictPending;
                next.current_owner = "claude".to_string();
            }
        }
        (Phase::CodeReviewVerdictPending, CollabEvent::VerdictGlobal { verdict, head_sha }) => {
            require_actor(actor, "claude")?;
            validate_coding_verdict(verdict)?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewDebatePending;
            next.current_owner = "codex".to_string();
        }
        (Phase::CodeReviewDebatePending, CollabEvent::CommentGlobal { head_sha }) => {
            require_actor(actor, "codex")?;
            next.last_head_sha = Some(head_sha.clone());
            next.phase = Phase::CodeReviewFinalPending;
            next.current_owner = "claude".to_string();
        }
        (Phase::CodeReviewFinalPending, CollabEvent::FinalReview { head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            if session.global_review_round >= MAX_GLOBAL_REVIEW_ROUNDS {
                next.phase = Phase::PrReadyPending;
                next.current_owner = "claude".to_string();
            } else {
                next.phase = Phase::CodeReviewCodexPending;
                next.current_owner = "codex".to_string();
            }
        }
        // ── v2: PR handoff ────────────────────────────────────────────────
        (Phase::PrReadyPending, CollabEvent::PrOpened { pr_url, head_sha }) => {
            require_actor(actor, "claude")?;
            next.last_head_sha = Some(head_sha.clone());
            next.pr_url = Some(pr_url.clone());
            next.phase = Phase::CodingComplete;
            next.current_owner = "claude".to_string();
        }
        // ── v2: failure is valid from any coding-active phase ─────────────
        (phase, CollabEvent::FailureReport { coding_failure }) if phase.is_coding_active() => {
            // Drift failures (prefix `branch_drift:`) may be emitted by either
            // agent because the non-owner often detects drift via its own git
            // ops. Any other failure must come from `current_owner` so an
            // off-turn agent cannot unilaterally abort the other's work.
            let is_drift = coding_failure.starts_with(BRANCH_DRIFT_PREFIX);
            if !is_drift && actor != session.current_owner {
                return Err(CollabError::NotYourTurn {
                    expected: session.current_owner.clone(),
                    got: actor.to_string(),
                });
            }
            next.coding_failure = Some(coding_failure.clone());
            next.phase = Phase::CodingFailed;
            next.current_owner = actor.to_string();
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
