//! Pure state machine for the bounded Claude↔Codex planning flow.

pub mod queue;

use std::fmt;

/// Maximum number of review cycles Codex may run on the canonical plan.
/// After this many reviews, Claude is forced into finalize regardless of the
/// verdict (she always gets the last word).
pub const MAX_REVIEW_ROUNDS: u8 = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    PlanParallelDrafts,
    PlanSynthesisPending,
    PlanCodexReviewPending,
    PlanClaudeFinalizePending,
    PlanLocked,
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Self::PlanParallelDrafts => "PlanParallelDrafts",
            Self::PlanSynthesisPending => "PlanSynthesisPending",
            Self::PlanCodexReviewPending => "PlanCodexReviewPending",
            Self::PlanClaudeFinalizePending => "PlanClaudeFinalizePending",
            Self::PlanLocked => "PlanLocked",
        };
        f.write_str(value)
    }
}

impl TryFrom<&str> for Phase {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        match value {
            "PlanParallelDrafts" => Ok(Self::PlanParallelDrafts),
            "PlanSynthesisPending" => Ok(Self::PlanSynthesisPending),
            "PlanCodexReviewPending" => Ok(Self::PlanCodexReviewPending),
            "PlanClaudeFinalizePending" => Ok(Self::PlanClaudeFinalizePending),
            "PlanLocked" => Ok(Self::PlanLocked),
            other => Err(format!("unknown collab phase: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollabSession {
    pub id: String,
    pub phase: Phase,
    pub current_owner: String,
    pub claude_draft_hash: Option<String>,
    pub codex_draft_hash: Option<String>,
    pub canonical_plan_hash: Option<String>,
    pub final_plan_hash: Option<String>,
    pub codex_review_verdict: Option<String>,
    pub review_round: u8,
}

impl CollabSession {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            phase: Phase::PlanParallelDrafts,
            current_owner: "claude".to_string(),
            claude_draft_hash: None,
            codex_draft_hash: None,
            canonical_plan_hash: None,
            final_plan_hash: None,
            codex_review_verdict: None,
            review_round: 0,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CollabEvent {
    SubmitDraft { content_hash: String },
    PublishCanonical { content_hash: String },
    SubmitReview { verdict: String },
    PublishFinal { content_hash: String },
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CollabError {
    #[error("not your turn: expected {expected}, got {got}")]
    NotYourTurn { expected: String, got: String },

    #[error("draft already submitted by {agent}")]
    AlreadySubmittedDraft { agent: String },

    #[error("invalid verdict value: {0}")]
    InvalidVerdictValue(String),

    #[error("wrong phase: expected {expected}, got {got}")]
    WrongPhase { expected: String, got: String },

    #[error("session is locked")]
    SessionLocked,
}

fn event_name(event: &CollabEvent) -> &'static str {
    match event {
        CollabEvent::SubmitDraft { .. } => "SubmitDraft",
        CollabEvent::PublishCanonical { .. } => "PublishCanonical",
        CollabEvent::SubmitReview { .. } => "SubmitReview",
        CollabEvent::PublishFinal { .. } => "PublishFinal",
    }
}

pub fn apply_event(
    session: &CollabSession,
    actor: &str,
    event: &CollabEvent,
) -> Result<CollabSession, CollabError> {
    if matches!(session.phase, Phase::PlanLocked) {
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
            if actor != "claude" {
                return Err(CollabError::NotYourTurn {
                    expected: "claude".to_string(),
                    got: actor.to_string(),
                });
            }
            next.canonical_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanCodexReviewPending;
            next.current_owner = "codex".to_string();
        }
        (Phase::PlanCodexReviewPending, CollabEvent::SubmitReview { verdict }) => {
            if actor != "codex" {
                return Err(CollabError::NotYourTurn {
                    expected: "codex".to_string(),
                    got: actor.to_string(),
                });
            }
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
            if actor != "claude" {
                return Err(CollabError::NotYourTurn {
                    expected: "claude".to_string(),
                    got: actor.to_string(),
                });
            }
            next.final_plan_hash = Some(content_hash.clone());
            next.phase = Phase::PlanLocked;
        }
        (Phase::PlanParallelDrafts, _) => {
            return Err(CollabError::WrongPhase {
                expected: "SubmitDraft".to_string(),
                got: event_name(event).to_string(),
            });
        }
        (Phase::PlanSynthesisPending, _) => {
            return Err(CollabError::WrongPhase {
                expected: "PublishCanonical".to_string(),
                got: event_name(event).to_string(),
            });
        }
        (Phase::PlanCodexReviewPending, _) => {
            return Err(CollabError::WrongPhase {
                expected: "SubmitReview".to_string(),
                got: event_name(event).to_string(),
            });
        }
        (Phase::PlanClaudeFinalizePending, _) => {
            return Err(CollabError::WrongPhase {
                expected: "PublishFinal".to_string(),
                got: event_name(event).to_string(),
            });
        }
        (Phase::PlanLocked, _) => return Err(CollabError::SessionLocked),
    }

    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> CollabSession {
        CollabSession::new("test-session")
    }

    fn draft(actor: &str, hash: &str, s: &CollabSession) -> CollabSession {
        apply_event(
            s,
            actor,
            &CollabEvent::SubmitDraft {
                content_hash: hash.to_string(),
            },
        )
        .unwrap()
    }

    fn canonical(hash: &str, s: &CollabSession) -> CollabSession {
        apply_event(
            s,
            "claude",
            &CollabEvent::PublishCanonical {
                content_hash: hash.to_string(),
            },
        )
        .unwrap()
    }

    fn review(verdict: &str, s: &CollabSession) -> CollabSession {
        apply_event(
            s,
            "codex",
            &CollabEvent::SubmitReview {
                verdict: verdict.to_string(),
            },
        )
        .unwrap()
    }

    #[test]
    fn test_parallel_drafts_both_submit_advances_phase() {
        let s = session();
        let s = draft("claude", "c1", &s);
        assert_eq!(s.phase, Phase::PlanParallelDrafts);
        let s = draft("codex", "c2", &s);
        assert_eq!(s.phase, Phase::PlanSynthesisPending);
        assert_eq!(s.current_owner, "claude");
    }

    #[test]
    fn test_duplicate_draft_rejected() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::SubmitDraft {
                content_hash: "c2".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            CollabError::AlreadySubmittedDraft {
                agent: "claude".to_string()
            }
        );
    }

    #[test]
    fn test_non_claude_cannot_publish_canonical() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let err = apply_event(
            &s,
            "codex",
            &CollabEvent::PublishCanonical {
                content_hash: "merged".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            CollabError::NotYourTurn {
                expected: "claude".to_string(),
                got: "codex".to_string()
            }
        );
    }

    #[test]
    fn test_codex_review_approve_advances_to_finalize() {
        for verdict in ["approve", "approve_with_minor_edits"] {
            let s = session();
            let s = draft("claude", "c1", &s);
            let s = draft("codex", "c2", &s);
            let s = canonical("canonical", &s);
            let s = review(verdict, &s);
            assert_eq!(s.phase, Phase::PlanClaudeFinalizePending);
            assert_eq!(s.codex_review_verdict.as_deref(), Some(verdict));
            assert_eq!(s.review_round, 1);
        }
    }

    #[test]
    fn test_request_changes_round_one_returns_to_synthesis() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("canonical-v1", &s);
        let s = review("request_changes", &s);

        assert_eq!(s.phase, Phase::PlanSynthesisPending);
        assert_eq!(s.current_owner, "claude");
        assert_eq!(s.review_round, 1);

        // Claude revises — publishes new canonical, back to review.
        let s = canonical("canonical-v2", &s);
        assert_eq!(s.phase, Phase::PlanCodexReviewPending);
        assert_eq!(s.canonical_plan_hash.as_deref(), Some("canonical-v2"));
    }

    #[test]
    fn test_request_changes_at_cap_forces_finalize() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("v1", &s);
        let s = review("request_changes", &s); // round 1, back to synthesis
        let s = canonical("v2", &s);
        let s = review("request_changes", &s); // round 2, hits MAX_REVIEW_ROUNDS

        assert_eq!(s.review_round, MAX_REVIEW_ROUNDS);
        assert_eq!(s.phase, Phase::PlanClaudeFinalizePending);
        assert_eq!(s.current_owner, "claude");
        assert_eq!(s.codex_review_verdict.as_deref(), Some("request_changes"));
    }

    #[test]
    fn test_finalize_always_locks() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("canonical", &s);
        let s = review("approve", &s);
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::PublishFinal {
                content_hash: "final".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::PlanLocked);
        assert_eq!(s.final_plan_hash.as_deref(), Some("final"));
    }

    #[test]
    fn test_finalize_locks_even_after_forced_finalize() {
        // Reach finalize via 2x request_changes, then publish final — still locks.
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("v1", &s);
        let s = review("request_changes", &s);
        let s = canonical("v2", &s);
        let s = review("request_changes", &s);

        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::PublishFinal {
                content_hash: "final".to_string(),
            },
        )
        .unwrap();
        assert_eq!(s.phase, Phase::PlanLocked);
    }

    #[test]
    fn test_wrong_phase_returns_error() {
        let s = session();
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::PublishCanonical {
                content_hash: "canonical".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            CollabError::WrongPhase {
                expected: "SubmitDraft".to_string(),
                got: "PublishCanonical".to_string()
            }
        );
    }

    #[test]
    fn test_invalid_verdict_rejected() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("canonical", &s);
        let err = apply_event(
            &s,
            "codex",
            &CollabEvent::SubmitReview {
                verdict: "looks good to me".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(
            err,
            CollabError::InvalidVerdictValue("looks good to me".to_string())
        );
    }

    #[test]
    fn test_locked_session_rejects_all_events() {
        let s = session();
        let s = draft("claude", "c1", &s);
        let s = draft("codex", "c2", &s);
        let s = canonical("canonical", &s);
        let s = review("approve", &s);
        let s = apply_event(
            &s,
            "claude",
            &CollabEvent::PublishFinal {
                content_hash: "final".to_string(),
            },
        )
        .unwrap();
        let err = apply_event(
            &s,
            "claude",
            &CollabEvent::SubmitDraft {
                content_hash: "x".to_string(),
            },
        )
        .unwrap_err();
        assert_eq!(err, CollabError::SessionLocked);
    }
}
