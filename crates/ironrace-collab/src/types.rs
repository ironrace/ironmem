//! Core types for the collaboration protocol state machine.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    PlanDraft,
    PlanReview,
    PlanFeedback,
    PlanRevised,
    PlanApproved,
    PlanEscalated,
}

impl Phase {
    pub fn as_str(&self) -> &'static str {
        match self {
            Phase::PlanDraft => "PlanDraft",
            Phase::PlanReview => "PlanReview",
            Phase::PlanFeedback => "PlanFeedback",
            Phase::PlanRevised => "PlanRevised",
            Phase::PlanApproved => "PlanApproved",
            Phase::PlanEscalated => "PlanEscalated",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "PlanDraft" => Some(Phase::PlanDraft),
            "PlanReview" => Some(Phase::PlanReview),
            "PlanFeedback" => Some(Phase::PlanFeedback),
            "PlanRevised" => Some(Phase::PlanRevised),
            "PlanApproved" => Some(Phase::PlanApproved),
            "PlanEscalated" => Some(Phase::PlanEscalated),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Agent {
    Claude,
    Codex,
}

impl Agent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Claude => "claude",
            Agent::Codex => "codex",
        }
    }

    pub fn other(&self) -> Agent {
        match self {
            Agent::Claude => Agent::Codex,
            Agent::Codex => Agent::Claude,
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "claude" => Some(Agent::Claude),
            "codex" => Some(Agent::Codex),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Topic {
    Plan,
    Review,
    Feedback,
    Approve,
    Reject,
    Escalation,
}

impl Topic {
    pub fn as_str(&self) -> &'static str {
        match self {
            Topic::Plan => "plan",
            Topic::Review => "review",
            Topic::Feedback => "feedback",
            Topic::Approve => "approve",
            Topic::Reject => "reject",
            Topic::Escalation => "escalation",
        }
    }

    pub fn from_name(s: &str) -> Option<Self> {
        match s {
            "plan" => Some(Topic::Plan),
            "review" => Some(Topic::Review),
            "feedback" => Some(Topic::Feedback),
            "approve" => Some(Topic::Approve),
            "reject" => Some(Topic::Reject),
            "escalation" => Some(Topic::Escalation),
            _ => None,
        }
    }
}
