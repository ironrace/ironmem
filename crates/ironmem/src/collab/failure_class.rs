//! Classification of `coding_failure` strings into recoverable ("tooling")
//! vs unrecoverable ("terminal") failures.
//!
//! A failure classifies as [`FailureClass::Tooling`] only when it starts
//! with one of [`super::RECOVERABLE_FAILURE_PREFIXES`] *and* carries a
//! non-empty detail suffix after the prefix — mirroring the "prefix + >=1
//! byte" rule already used by [`super::OFF_TURN_FAILURE_PREFIXES`]. A bare
//! recoverable prefix with nothing after it, `branch_drift:`,
//! `subagent_failure:`, any unrecognized string, and the empty string all
//! classify as [`FailureClass::Terminal`].
//!
//! This module only classifies. The wiring lives in
//! [`crate::collab::state_machine::apply_event`], whose `FailureReport` arm
//! matches on the result: `Tooling` parks the session in its current phase
//! and hands the turn to the counterpart agent, `Terminal` transitions it to
//! `CodingFailed`. `resume_eligibility` calls back into [`classify`] to
//! decide whether a `CodingFailed` session may resume.

use super::RECOVERABLE_FAILURE_PREFIXES;

/// Classification of a `coding_failure` string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureClass {
    /// A recoverable tooling failure (e.g. a failed `git push`, a denied
    /// sandbox operation) — worth retrying rather than aborting the collab
    /// session.
    Tooling,
    /// An unrecoverable failure — the collab session should end.
    Terminal,
}

/// Classify a `coding_failure` string as [`FailureClass::Tooling`] or
/// [`FailureClass::Terminal`].
///
/// `Tooling` requires both a recognized recoverable prefix (see
/// [`RECOVERABLE_FAILURE_PREFIXES`]) and a non-empty detail suffix; a bare
/// prefix with nothing after it classifies as `Terminal`, same as any
/// unrecognized string (including `branch_drift:` and `subagent_failure:`,
/// neither of which is in the recoverable set) and the empty string.
pub fn classify(coding_failure: &str) -> FailureClass {
    let is_recoverable = RECOVERABLE_FAILURE_PREFIXES.iter().any(|prefix| {
        coding_failure
            .strip_prefix(prefix)
            .is_some_and(|suffix| !suffix.is_empty())
    });

    if is_recoverable {
        FailureClass::Tooling
    } else {
        FailureClass::Terminal
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collab::{
        BRANCH_DRIFT_PREFIX, CODEX_DISPATCH_FAILED_PREFIX, DISK_FULL_PREFIX,
        GIT_COMMIT_FAILED_PREFIX, GIT_PUSH_FAILED_PREFIX, NETWORK_FAILED_PREFIX,
        RECOVERABLE_FAILURE_PREFIXES, SANDBOX_DENIED_PREFIX,
    };

    #[test]
    fn all_six_recoverable_prefixes_are_covered_by_the_fixture() {
        // Guards against the fixture list below silently drifting from the
        // canonical prefix list if a prefix is ever added or removed.
        assert_eq!(RECOVERABLE_FAILURE_PREFIXES.len(), 6);
    }

    #[test]
    fn each_recoverable_prefix_with_a_detail_suffix_classifies_tooling() {
        for prefix in [
            GIT_COMMIT_FAILED_PREFIX,
            GIT_PUSH_FAILED_PREFIX,
            SANDBOX_DENIED_PREFIX,
            DISK_FULL_PREFIX,
            NETWORK_FAILED_PREFIX,
            CODEX_DISPATCH_FAILED_PREFIX,
        ] {
            let with_detail = format!("{prefix} something went wrong");
            assert_eq!(
                classify(&with_detail),
                FailureClass::Tooling,
                "expected {with_detail:?} to classify Tooling"
            );
        }
    }

    #[test]
    fn each_recoverable_prefix_bare_with_no_suffix_classifies_terminal() {
        for prefix in [
            GIT_COMMIT_FAILED_PREFIX,
            GIT_PUSH_FAILED_PREFIX,
            SANDBOX_DENIED_PREFIX,
            DISK_FULL_PREFIX,
            NETWORK_FAILED_PREFIX,
            CODEX_DISPATCH_FAILED_PREFIX,
        ] {
            assert_eq!(
                classify(prefix),
                FailureClass::Terminal,
                "expected bare {prefix:?} (no suffix) to classify Terminal"
            );
        }
    }

    #[test]
    fn branch_drift_classifies_terminal() {
        let failure = format!("{BRANCH_DRIFT_PREFIX} coding branch does not match session");
        assert_eq!(classify(&failure), FailureClass::Terminal);
    }

    #[test]
    fn subagent_failure_classifies_terminal() {
        assert_eq!(
            classify("subagent_failure: task 3 crashed"),
            FailureClass::Terminal
        );
    }

    #[test]
    fn unknown_string_classifies_terminal() {
        assert_eq!(classify("gh_auth: not logged in"), FailureClass::Terminal);
        assert_eq!(
            classify("some entirely unrelated failure text"),
            FailureClass::Terminal
        );
    }

    #[test]
    fn empty_string_classifies_terminal() {
        assert_eq!(classify(""), FailureClass::Terminal);
    }
}
