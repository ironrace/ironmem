//! Pure state machine for the Claude↔Codex planning collaboration loop.
//!
//! No I/O, no database — all transitions are deterministic given a session snapshot.
//! Callers load a `CollabSession` from the DB, call methods, then persist the result.

use crate::types::{Agent, Phase, Topic};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CollabError {
    #[error("not your turn: expected {expected}, got {got}")]
    NotYourTurn { expected: String, got: String },

    #[error("content hash mismatch: cannot approve a proposal that is not current")]
    HashMismatch,

    #[error("this content hash was already rejected; send a revised plan first")]
    AlreadyRejected,

    #[error("invalid transition from phase {0}")]
    InvalidTransition(String),
}

/// In-memory snapshot of a collaboration session — load from DB, mutate, persist.
#[derive(Debug, Clone)]
pub struct CollabSession {
    pub id: String,
    pub phase: Phase,
    pub current_owner: Agent,
    pub round: u32,
    pub max_rounds: u32,
    pub claude_ok: bool,
    pub codex_ok: bool,
    /// SHA-256 (or any stable hash) of the current proposal content.
    pub content_hash: Option<String>,
    /// Hashes that have been explicitly rejected via `Topic::Reject`.
    /// Approving a rejected hash is blocked.
    pub rejected_hashes: Vec<String>,
}

impl CollabSession {
    pub fn new(id: impl Into<String>) -> Self {
        CollabSession {
            id: id.into(),
            phase: Phase::PlanDraft,
            current_owner: Agent::Claude,
            round: 0,
            max_rounds: 5,
            claude_ok: false,
            codex_ok: false,
            content_hash: None,
            rejected_hashes: Vec::new(),
        }
    }

    /// Validate and apply a `send` action. Returns the new phase on success.
    ///
    /// Rules:
    /// - Sender must be the current owner.
    /// - If round >= max_rounds, escalate immediately.
    /// - Transitions follow the planning loop state table.
    pub fn on_send(
        &mut self,
        sender: &Agent,
        topic: &Topic,
        content_hash: Option<&str>,
    ) -> Result<&Phase, CollabError> {
        // Terminal states block all further sends
        if matches!(self.phase, Phase::PlanApproved | Phase::PlanEscalated) {
            return Err(CollabError::InvalidTransition(
                self.phase.as_str().to_string(),
            ));
        }

        // Turn enforcement
        if sender != &self.current_owner {
            return Err(CollabError::NotYourTurn {
                expected: self.current_owner.as_str().to_string(),
                got: sender.as_str().to_string(),
            });
        }

        // Escalation check (before transition)
        if self.round >= self.max_rounds {
            self.phase = Phase::PlanEscalated;
            return Ok(&self.phase);
        }

        // Handle reject: mark current hash as rejected, reset approval bits
        if topic == &Topic::Reject {
            if let Some(hash) = &self.content_hash {
                self.rejected_hashes.push(hash.clone());
            }
            self.claude_ok = false;
            self.codex_ok = false;
            self.content_hash = None;
            // Owner flips to the other agent to send a new plan
            self.current_owner = sender.other();
            return Ok(&self.phase);
        }

        // Phase-specific transitions
        match (&self.phase, sender, topic) {
            // Claude writes initial plan → moves to review
            (Phase::PlanDraft, Agent::Claude, Topic::Plan) => {
                self.content_hash = content_hash.map(str::to_string);
                self.claude_ok = false;
                self.codex_ok = false;
                self.phase = Phase::PlanReview;
                self.current_owner = Agent::Codex;
            }

            // Codex sends feedback after reviewing → back to claude
            (Phase::PlanReview, Agent::Codex, Topic::Feedback)
            | (Phase::PlanRevised, Agent::Codex, Topic::Feedback) => {
                self.phase = Phase::PlanFeedback;
                self.current_owner = Agent::Claude;
            }

            // Claude revises after feedback → round increments
            (Phase::PlanFeedback, Agent::Claude, Topic::Plan) => {
                self.content_hash = content_hash.map(str::to_string);
                self.claude_ok = false;
                self.codex_ok = false;
                self.round += 1;

                // Escalate if round limit just reached
                if self.round >= self.max_rounds {
                    self.phase = Phase::PlanEscalated;
                } else {
                    self.phase = Phase::PlanRevised;
                    self.current_owner = Agent::Codex;
                }
            }

            // Any other combination is invalid
            _ => {
                return Err(CollabError::InvalidTransition(format!(
                    "{} by {} with topic {}",
                    self.phase.as_str(),
                    sender.as_str(),
                    topic.as_str(),
                )));
            }
        }

        Ok(&self.phase)
    }

    /// Register an approval from one agent for a specific content hash.
    ///
    /// Both agents must approve the same `content_hash` for consensus.
    /// Returns `true` if consensus is reached (both approved).
    pub fn on_approve(&mut self, agent: &Agent, hash: &str) -> Result<bool, CollabError> {
        // Terminal states block approval
        if matches!(self.phase, Phase::PlanApproved | Phase::PlanEscalated) {
            return Err(CollabError::InvalidTransition(
                self.phase.as_str().to_string(),
            ));
        }

        // Reject if this hash was previously rejected
        if self.rejected_hashes.iter().any(|h| h == hash) {
            return Err(CollabError::AlreadyRejected);
        }

        // Hash must match the current proposal
        match &self.content_hash {
            None => return Err(CollabError::HashMismatch),
            Some(current) if current != hash => return Err(CollabError::HashMismatch),
            _ => {}
        }

        match agent {
            Agent::Claude => self.claude_ok = true,
            Agent::Codex => self.codex_ok = true,
        }

        let consensus = self.claude_ok && self.codex_ok;
        if consensus {
            self.phase = Phase::PlanApproved;
        }

        Ok(consensus)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Agent, Topic};

    fn session() -> CollabSession {
        CollabSession::new("test-session")
    }

    // ── Happy-path transitions ──────────────────────────────────────────────

    #[test]
    fn new_session_starts_in_plan_draft_owned_by_claude() {
        let s = session();
        assert_eq!(s.phase, Phase::PlanDraft);
        assert_eq!(s.current_owner, Agent::Claude);
        assert_eq!(s.round, 0);
        assert!(!s.claude_ok);
        assert!(!s.codex_ok);
    }

    #[test]
    fn claude_sends_plan_moves_to_plan_review() {
        let mut s = session();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("hash1"))
            .unwrap();
        assert_eq!(s.phase, Phase::PlanReview);
        assert_eq!(s.current_owner, Agent::Codex);
        assert_eq!(s.content_hash.as_deref(), Some("hash1"));
    }

    #[test]
    fn codex_sends_feedback_moves_to_plan_feedback() {
        let mut s = session();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h1")).unwrap();
        s.on_send(&Agent::Codex, &Topic::Feedback, None).unwrap();
        assert_eq!(s.phase, Phase::PlanFeedback);
        assert_eq!(s.current_owner, Agent::Claude);
    }

    #[test]
    fn claude_revises_moves_to_plan_revised_and_increments_round() {
        let mut s = session();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h1")).unwrap();
        s.on_send(&Agent::Codex, &Topic::Feedback, None).unwrap();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h2")).unwrap();
        assert_eq!(s.phase, Phase::PlanRevised);
        assert_eq!(s.current_owner, Agent::Codex);
        assert_eq!(s.round, 1);
        assert_eq!(s.content_hash.as_deref(), Some("h2"));
    }

    #[test]
    fn codex_feedback_on_revised_loops_back() {
        let mut s = session();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h1")).unwrap();
        s.on_send(&Agent::Codex, &Topic::Feedback, None).unwrap();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h2")).unwrap();
        s.on_send(&Agent::Codex, &Topic::Feedback, None).unwrap();
        assert_eq!(s.phase, Phase::PlanFeedback);
        assert_eq!(s.current_owner, Agent::Claude);
    }

    #[test]
    fn both_approve_same_hash_yields_plan_approved() {
        let mut s = session();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("final"))
            .unwrap();
        // Codex approves
        let consensus = s.on_approve(&Agent::Codex, "final").unwrap();
        assert!(!consensus, "only one side approved");
        assert_eq!(s.phase, Phase::PlanReview);
        // Claude approves
        let consensus = s.on_approve(&Agent::Claude, "final").unwrap();
        assert!(consensus, "both approved — should be consensus");
        assert_eq!(s.phase, Phase::PlanApproved);
    }

    // ── Escalation ──────────────────────────────────────────────────────────

    #[test]
    fn escalates_when_round_reaches_max_rounds() {
        let mut s = session();
        s.max_rounds = 2;
        // Round 0 → 1
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h0")).unwrap();
        s.on_send(&Agent::Codex, &Topic::Feedback, None).unwrap();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h1")).unwrap(); // round becomes 1
        assert_eq!(s.phase, Phase::PlanRevised);

        s.on_send(&Agent::Codex, &Topic::Feedback, None).unwrap();
        let phase = s.on_send(&Agent::Claude, &Topic::Plan, Some("h2")).unwrap(); // round becomes 2 → escalate
        assert_eq!(*phase, Phase::PlanEscalated);
        assert_eq!(s.phase, Phase::PlanEscalated);
    }

    // ── Error cases ─────────────────────────────────────────────────────────

    #[test]
    fn wrong_agent_send_returns_not_your_turn() {
        let mut s = session();
        let err = s.on_send(&Agent::Codex, &Topic::Plan, None).unwrap_err();
        assert!(matches!(err, CollabError::NotYourTurn { .. }));
        if let CollabError::NotYourTurn { expected, got } = err {
            assert_eq!(expected, "claude");
            assert_eq!(got, "codex");
        }
    }

    #[test]
    fn approve_with_wrong_hash_returns_hash_mismatch() {
        let mut s = session();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("real-hash"))
            .unwrap();
        let err = s.on_approve(&Agent::Codex, "wrong-hash").unwrap_err();
        assert_eq!(err, CollabError::HashMismatch);
    }

    #[test]
    fn approve_when_no_content_hash_returns_hash_mismatch() {
        let mut s = session(); // content_hash is None initially
        let err = s.on_approve(&Agent::Codex, "anything").unwrap_err();
        assert_eq!(err, CollabError::HashMismatch);
    }

    #[test]
    fn approve_rejected_hash_returns_already_rejected() {
        let mut s = session();
        // Claude sends plan, Codex rejects it
        s.on_send(&Agent::Claude, &Topic::Plan, Some("rejected-hash"))
            .unwrap();
        s.on_send(&Agent::Codex, &Topic::Reject, None).unwrap();
        // Now Claude tries to approve the rejected hash
        let err = s.on_approve(&Agent::Claude, "rejected-hash").unwrap_err();
        assert_eq!(err, CollabError::AlreadyRejected);
    }

    #[test]
    fn invalid_transition_from_terminal_state() {
        let mut s = session();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h")).unwrap();
        s.on_approve(&Agent::Claude, "h").unwrap();
        s.on_approve(&Agent::Codex, "h").unwrap(); // → PlanApproved
        let err = s
            .on_send(&Agent::Claude, &Topic::Plan, Some("h2"))
            .unwrap_err();
        assert!(matches!(err, CollabError::InvalidTransition(_)));
    }

    #[test]
    fn reject_clears_approval_bits_and_flips_owner() {
        let mut s = session();
        s.on_send(&Agent::Claude, &Topic::Plan, Some("h1")).unwrap();
        s.on_approve(&Agent::Claude, "h1").unwrap(); // claude approved
                                                     // Codex rejects
        s.on_send(&Agent::Codex, &Topic::Reject, None).unwrap();
        assert!(!s.claude_ok);
        assert!(!s.codex_ok);
        assert!(s.content_hash.is_none());
        // Owner should now be claude (send a new plan)
        assert_eq!(s.current_owner, Agent::Claude);
    }
}
