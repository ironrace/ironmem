use std::fmt;
use std::str::FromStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    // Planning (v1)
    PlanParallelDrafts,
    PlanSynthesisPending,
    PlanCopilotReviewPending,
    PlanFinalizePending,
    PlanLocked,
    // Coding (v3) — batch implementation. The selected implementer
    // orchestrates per-task subagents (via `iron-build`, or directly when
    // `execution_mode` says so) entirely on its side. The single transition
    // out is `implementation_done`, which jumps straight to global review.
    CodeImplementPending,
    // Coding (v3) — global review, 3-phase linear:
    //   CodeReviewFixGlobalPending (the copilot reads the raw
    //   post-implementation diff and applies fixes directly; no pilot
    //   pre-clean)
    //   → CodeReviewLocalPending (the pilot audits the copilot's commits via
    //     `/ultrareview-local` and catches code-quality issues both agents
    //     missed)
    //   → CodeReviewFinalPending (the pilot opens the PR)
    // "Pilot" is the session's `pilot` agent and "copilot" is its
    // counterpart; the split is per-session, not a fixed Claude/Codex
    // assignment.
    // Note: these three are listed in legacy order, not transition order —
    // variant order carries no meaning here. The wire form comes from
    // `wire_name`, and the transition order is enforced by
    // `state_machine::mod::apply_event`.
    CodeReviewLocalPending,
    CodeReviewFixGlobalPending,
    CodeReviewFinalPending,
    // Coding (v3) — terminal
    CodingComplete,
    // `CodingFailed` must remain the LAST declared variant: the
    // compile-time completeness proof below anchors `ALL_PHASES.len()` on
    // its discriminant. New variants go *before* it. Declaration order is
    // otherwise cosmetic — the wire form comes from `wire_name`, never from
    // the discriminant.
    CodingFailed,
}

/// Every `Phase` variant, in declaration order. The parse/serialize tests
/// iterate this rather than `PHASE_NAMES`, so a variant missing a table row
/// is a test failure instead of being invisible.
///
/// Completeness is proved at compile time by the `const` block below, not by
/// review: each slot must hold the variant whose discriminant equals its
/// index, and the length must equal the last variant's discriminant plus
/// one. Inserting a variant anywhere shifts a discriminant and breaks one of
/// those two assertions.
const ALL_PHASES: &[Phase] = &[
    Phase::PlanParallelDrafts,
    Phase::PlanSynthesisPending,
    Phase::PlanCopilotReviewPending,
    Phase::PlanFinalizePending,
    Phase::PlanLocked,
    Phase::CodeImplementPending,
    Phase::CodeReviewLocalPending,
    Phase::CodeReviewFixGlobalPending,
    Phase::CodeReviewFinalPending,
    Phase::CodingComplete,
    Phase::CodingFailed,
];

const _: () = {
    assert!(
        ALL_PHASES.len() == Phase::CodingFailed as usize + 1,
        "ALL_PHASES must list every Phase variant (CodingFailed must stay last)"
    );
    let mut i = 0;
    while i < ALL_PHASES.len() {
        assert!(
            ALL_PHASES[i] as usize == i,
            "ALL_PHASES must list every Phase variant once, in declaration order"
        );
        i += 1;
    }
    assert!(
        PHASE_NAMES.len() == ALL_PHASES.len(),
        "PHASE_NAMES must have exactly one row per Phase variant"
    );
};

/// Parse table for the DB string forms. `Display` does **not** read this —
/// it delegates to the exhaustive [`Phase::wire_name`] match, so a variant
/// with no row here can never serialize as a placeholder. This table is the
/// reverse direction only (`FromStr`); the length assertion above and the
/// round-trip test below together pin it as an exact inverse of `wire_name`.
///
/// String values are byte-identical to what the old match-based `Display`
/// and `TryFrom` produced — changing them would corrupt stored sessions.
///
/// Pre-1.0 protocol: in-flight sessions parked at the removed
/// `CodeReviewFixPending` or `CodeFinalPending` phases will fail to load
/// after the batch-implementation refactor. There is no migration; the
/// expectation is that all dev sessions are short-lived and the operator
/// can restart any abandoned ones.
const PHASE_NAMES: &[(Phase, &str)] = &[
    (Phase::PlanParallelDrafts, "PlanParallelDrafts"),
    (Phase::PlanSynthesisPending, "PlanSynthesisPending"),
    (Phase::PlanCopilotReviewPending, "PlanCodexReviewPending"),
    (Phase::PlanFinalizePending, "PlanClaudeFinalizePending"),
    (Phase::PlanLocked, "PlanLocked"),
    (Phase::CodeImplementPending, "CodeImplementPending"),
    (Phase::CodeReviewLocalPending, "CodeReviewLocalPending"),
    (
        Phase::CodeReviewFixGlobalPending,
        "CodeReviewFixGlobalPending",
    ),
    (Phase::CodeReviewFinalPending, "CodeReviewFinalPending"),
    (Phase::CodingComplete, "CodingComplete"),
    (Phase::CodingFailed, "CodingFailed"),
];

impl Phase {
    /// The DB/wire string for this variant — the single source of truth for
    /// serialization, which `Display` merely forwards to.
    ///
    /// Written as an exhaustive `match` (no `_` arm) on purpose: adding a
    /// `Phase` variant is a **compile error** here until it is given a wire
    /// string. That is what closes the old failure mode where a table-lookup
    /// `Display` fell back to a placeholder, `save_session` wrote that
    /// placeholder into `collab_sessions.phase` (a column with no CHECK
    /// constraint), and the next `load_session_record` could never parse it
    /// back — leaving the session permanently unloadable.
    ///
    /// Strings are byte-identical to the historical ones, including the two
    /// variants whose Rust names were later genericized
    /// (`PlanCopilotReviewPending` → `"PlanCodexReviewPending"`,
    /// `PlanFinalizePending` → `"PlanClaudeFinalizePending"`). Changing any
    /// of them corrupts stored sessions.
    pub const fn wire_name(&self) -> &'static str {
        match self {
            Self::PlanParallelDrafts => "PlanParallelDrafts",
            Self::PlanSynthesisPending => "PlanSynthesisPending",
            Self::PlanCopilotReviewPending => "PlanCodexReviewPending",
            Self::PlanFinalizePending => "PlanClaudeFinalizePending",
            Self::PlanLocked => "PlanLocked",
            Self::CodeImplementPending => "CodeImplementPending",
            Self::CodeReviewLocalPending => "CodeReviewLocalPending",
            Self::CodeReviewFixGlobalPending => "CodeReviewFixGlobalPending",
            Self::CodeReviewFinalPending => "CodeReviewFinalPending",
            Self::CodingComplete => "CodingComplete",
            Self::CodingFailed => "CodingFailed",
        }
    }

    /// True for phases that permanently end the session. `wait_my_turn` uses
    /// a dynamic terminal set: `PlanLocked` is terminal pre-`task_list`, and
    /// `{CodingComplete, CodingFailed}` is the terminal set post-`task_list`.
    /// This helper returns only the permanently-terminal cases; callers
    /// responsible for the dynamic set check `task_list` on the session.
    pub fn is_coding_terminal(&self) -> bool {
        matches!(self, Self::CodingComplete | Self::CodingFailed)
    }

    /// True for the one terminal phase that releases its repository-and-branch
    /// start slot before `collab_end`. Only `CodingComplete` qualifies:
    /// attestation is a human step of unbounded duration, so holding the slot
    /// for it would block the next session on that branch indefinitely.
    ///
    /// `CodingFailed` deliberately keeps its slot — it stays resumable, and the
    /// resume guard refuses a scope owned by a newer live session, so releasing
    /// it would let a replayed `collab_start` strand the failed session's plan
    /// and recovery columns. Kept in lockstep with the `phase <> 'CodingComplete'`
    /// predicate in [`crate::collab::queue::find_active_session_by_repo_branch`].
    pub fn releases_start_slot(&self) -> bool {
        matches!(self, Self::CodingComplete)
    }

    /// True if the session is currently inside the v3 coding loop. Used by
    /// `collab_end` to reject early-end calls.
    pub fn is_coding_active(&self) -> bool {
        matches!(
            self,
            Self::CodeImplementPending
                | Self::CodeReviewLocalPending
                | Self::CodeReviewFixGlobalPending
                | Self::CodeReviewFinalPending
        )
    }

    /// Whether `session_handoff { force_reissue: true }` may act on a session in
    /// this phase.
    ///
    /// Phase *policy*, so it lives with the phase rather than in the MCP handler
    /// that happens to enforce it — beside [`Self::is_coding_active`], which
    /// `collab_end` reads for the same shape of question. It sat in
    /// `mcp::tools::handoff` and had two readers already (the gate, and
    /// `collab_status`'s `reclaimable` verdict, which reached across into a
    /// sibling transport module to get it); the next reader would have been a
    /// third, and a rule about phases answerable only by calling into the
    /// transport layer is a rule that gets copied instead of called.
    ///
    /// # Why this is an exhaustive `match` and not a `matches!`
    ///
    /// The refused phases share one property: they wait on a **human** and write
    /// nothing to any agent-driven activity signal while they do, so a session
    /// parked in one reads as maximally stale no matter how alive its holder is.
    /// `collab_end`'s abandon gate accepts that false-positive risk because a wrong
    /// seal is loud and terminal. `force_reissue` cannot: a wrong verdict here hands
    /// an eviction capability to a caller acting against a process that never died,
    /// silently.
    ///
    /// This began as `matches!(phase, PlanLocked | CodingComplete)`, and that shape
    /// is the bug rather than a style choice: a `matches!` against a two-variant
    /// pattern makes every **future** `Phase` default to *admitted* — the permissive
    /// answer, for a capability that evicts live processes — and it compiles clean.
    /// `CodingFailed` was already missing for exactly that reason. An exhaustive
    /// `match` that names every variant on both arms makes a new phase a compile
    /// error, which forces the question to be answered rather than defaulted. Do not
    /// collapse either arm into a `_`.
    ///
    /// `collab_session.rs`'s `PHASE_ENDABILITY` / `PHASE_OWNER_REQUIRED` tables are
    /// the same discipline from the other side; the test-side twin of this function,
    /// `PHASE_FORCE_REISSUE_ADMITS`, spells the answers out independently so a typo
    /// here fails a row rather than silently redefining the rule.
    pub fn admits_forced_reissue(&self) -> bool {
        match self {
            // Waiting on the pilot's `task_list` send. `docs/COLLAB.md` is explicit
            // that this phase is autonomous, but it is still a *wait*: nothing
            // writes an agent-driven signal until the send lands.
            Self::PlanLocked => false,
            // Terminal, waiting on operator attestation — a human step of unbounded
            // duration by construction.
            Self::CodingComplete => false,
            // [`Self::is_coding_terminal`] yet deliberately kept resumable: it
            // waits for a human to call `collab_resume` and writes nothing
            // while it waits. Identical shape to the two above, and
            // it was missed on the first pass — a genuine tooling-class failure,
            // aged out, was admitted.
            Self::CodingFailed => false,
            // Every remaining phase is agent-driven: it advances through
            // `apply_event` → `save_session` (which stamps
            // `collab_sessions.updated_at`), and it waits on an *agent*, not on a
            // human — so six hours of total silence in one of these is anomalous,
            // which is what makes the staleness gate meaningful here.
            //
            // "Its normal traffic writes `messages`" was the rationale here, and it
            // is not true of `CodeImplementPending`. `handle_collab_status`'s own
            // comment says the opposite in as many words — a long
            // `CodeImplementPending` batch "files checkpoints without touching the
            // session row" — and that is the whole reason
            // `session_last_activity` counts `collab_checkpoints.updated_at` as a
            // term at all. The claim was load-bearing for the wrong reason: it made
            // this arm look like it rested on a signal that fires continuously,
            // when what it actually rests on is that *some* agent-driven term does.
            //
            // So the residual is named rather than argued away: an implementer that
            // hangs — a subagent wedged on a network call, a review loop that never
            // returns — writes no message, files no checkpoint, and stamps no
            // session row, and after six hours a `force_reissue` against it is
            // admitted while the process is still resident. That is a real
            // false-positive window and it is accepted here, because it is the
            // *rescue* case this feature exists for: an implementer silent for six
            // hours is the single most common wedge, refusing it would leave the
            // severed-chain lock with no remedy but `collab_end { abandon: true }`,
            // and the eviction is still not this call's — the successor's claim is
            // (R1). Narrowing it, if it ever needs narrowing, belongs in the
            // checkpoint cadence (a batch that heartbeats cannot look dead), not in
            // a longer threshold here: `COLLAB_DEAD_SESSION_SECS` is shared with
            // the abandon gate.
            Self::PlanParallelDrafts
            | Self::PlanSynthesisPending
            | Self::PlanCopilotReviewPending
            | Self::PlanFinalizePending
            | Self::CodeImplementPending
            | Self::CodeReviewLocalPending
            | Self::CodeReviewFixGlobalPending
            | Self::CodeReviewFinalPending => true,
        }
    }

    /// The single `CollabEvent` variant each active phase expects. Used by the
    /// catch-all `WrongPhase` arm to build a uniform error message. Terminal
    /// phases return a placeholder that the catch-all never reaches because
    /// `CodingComplete`/`CodingFailed` short-circuit to `SessionLocked` first.
    pub fn expected_event(&self) -> &'static str {
        match self {
            Self::PlanParallelDrafts => "SubmitDraft",
            Self::PlanSynthesisPending => "PublishCanonical",
            Self::PlanCopilotReviewPending => "SubmitReview",
            Self::PlanFinalizePending => "PublishFinal",
            Self::PlanLocked => "SubmitTaskList",
            Self::CodeImplementPending => "ImplementationDone",
            Self::CodeReviewLocalPending => "ReviewLocal",
            Self::CodeReviewFixGlobalPending => "CodeReviewFixGlobal",
            Self::CodeReviewFinalPending => "FinalReview",
            Self::CodingComplete | Self::CodingFailed => "SessionLocked",
        }
    }
}

impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.wire_name())
    }
}

impl FromStr for Phase {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        PHASE_NAMES
            .iter()
            .find(|(_, n)| *n == s)
            .map(|(p, _)| *p)
            .ok_or_else(|| format!("unknown collab phase: {s}"))
    }
}

impl TryFrom<&str> for Phase {
    type Error = String;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        value.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_renamed_plan_copilot_review_pending() {
        // Verify that the renamed variant still serializes to the original wire string
        let phase = Phase::PlanCopilotReviewPending;
        assert_eq!(phase.to_string(), "PlanCodexReviewPending");
    }

    #[test]
    fn test_renamed_plan_finalize_pending() {
        // Verify that the renamed variant still serializes to the original wire string
        let phase = Phase::PlanFinalizePending;
        assert_eq!(phase.to_string(), "PlanClaudeFinalizePending");
    }

    #[test]
    fn test_parse_plan_codex_review_pending() {
        // Verify that parsing the original wire string produces the renamed variant
        let phase: Phase = "PlanCodexReviewPending".parse().expect("parse failed");
        assert_eq!(phase, Phase::PlanCopilotReviewPending);
    }

    #[test]
    fn test_parse_plan_claude_finalize_pending() {
        // Verify that parsing the original wire string produces the renamed variant
        let phase: Phase = "PlanClaudeFinalizePending".parse().expect("parse failed");
        assert_eq!(phase, Phase::PlanFinalizePending);
    }

    /// Round-trip every variant through the DB encoding.
    ///
    /// Iterates `ALL_PHASES` (the variant set), deliberately **not**
    /// `PHASE_NAMES`. Driving the loop from the parse table would make the
    /// test self-referential — `Display` would be checked against the very
    /// row it was read from, and a variant with no row at all would simply
    /// never be visited. Iterating the variant set instead means a missing
    /// or mismatched `PHASE_NAMES` row fails here, and `ALL_PHASES` itself
    /// is proved complete by the `const` assertions in this module.
    #[test]
    fn test_all_phases_round_trip() {
        for phase in ALL_PHASES {
            let as_string = phase.to_string();
            assert_eq!(
                as_string,
                phase.wire_name(),
                "Display must forward to wire_name for {phase:?}"
            );

            let parsed: Phase = as_string
                .parse()
                .unwrap_or_else(|e| panic!("{phase:?} has no PHASE_NAMES row: {e}"));
            assert_eq!(
                parsed, *phase,
                "Round-trip failed for phase {phase:?}: got {parsed:?} after parsing {as_string}"
            );
        }
    }

    /// The inverse direction: no `PHASE_NAMES` row may be stale (naming a
    /// wire string that `wire_name` no longer emits). Together with the
    /// `const` length assertion and the round-trip above, this pins the
    /// table as an exact inverse of `wire_name`.
    #[test]
    fn test_phase_names_rows_match_wire_names() {
        for (phase, name) in PHASE_NAMES {
            assert_eq!(
                phase.wire_name(),
                *name,
                "stale PHASE_NAMES row for {phase:?}"
            );
        }
    }
}
