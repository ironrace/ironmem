//! The `agent:*` GitHub label vocabulary — build-ladder rung 6.
//!
//! The spec's *Migration* section defines exactly three labels, and their
//! whole point is that **their resume semantics differ**:
//!
//! | Label | Meaning | Resume |
//! |---|---|---|
//! | `agent:ready` | Opted in, eligible for dispatch | — |
//! | `agent:blocked` | Awaiting a human | **Auto-resumes** on a newer human comment |
//! | `agent:exhausted` | Per-issue attempt cap hit | **Never self-resumes** |
//!
//! Two blocked states rather than one, "because a question that has been
//! answered should flow again on its own, whereas work the system already
//! proved it cannot finish must not silently retry".
//!
//! # Why the label is the eligibility gate, not a drawer
//!
//! [`eligibility`] reads the *labels* rather than lineage state because the
//! spec makes the label the human's control surface: `agent:exhausted`
//! "never self-resumes. Only a human re-labeling it retries." A drawer-based
//! gate would be un-overridable from GitHub, which is the one place the
//! human is guaranteed to be looking. Lineage still bounds the work (rung
//! 4's cumulative attempt cap); the label is what a human can *undo*.
//!
//! # Why the transitions are exclusive
//!
//! An issue carrying both `agent:ready` and `agent:exhausted` has no defined
//! meaning, and the two answers to "may this be dispatched?" are opposite.
//! [`plan_exclusive`] therefore always produces a plan that removes every
//! `agent:*` label the issue currently carries except the target — including
//! the case where the target is `None` (a merged issue carries none, so the
//! Lead cannot re-pick work that is already done).
//!
//! Everything in this module except [`ensure_labels`] and
//! [`set_exclusive_label`] is pure, so the transition rules are tested
//! without a GitHub round-trip.

use serde::{Deserialize, Serialize};

use super::gh::{GhRunner, LABEL_ALREADY_EXISTS_MARKERS};
use super::{validate_repo, IssueRef};
use crate::error::MemoryError;

/// The namespace every Autopilot label lives in. Anything outside it is
/// another system's label and is never added or removed by this module.
pub const LABEL_PREFIX: &str = "agent:";

/// One of the spec's three `agent:*` labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentLabel {
    Ready,
    Blocked,
    Exhausted,
}

impl AgentLabel {
    /// Every label this module manages. Iterated by [`ensure_labels`] and by
    /// [`plan_exclusive`], so adding a variant cannot leave one un-created or
    /// un-removed.
    pub const ALL: [AgentLabel; 3] = [
        AgentLabel::Ready,
        AgentLabel::Blocked,
        AgentLabel::Exhausted,
    ];

    /// The literal GitHub label name.
    pub fn as_str(self) -> &'static str {
        match self {
            AgentLabel::Ready => "agent:ready",
            AgentLabel::Blocked => "agent:blocked",
            AgentLabel::Exhausted => "agent:exhausted",
        }
    }

    /// Parse a GitHub label name back into a variant. Returns `None` for any
    /// label outside this module's vocabulary — including other `agent:*`
    /// names, which are somebody else's and must be left alone.
    ///
    /// Compared case-insensitively because GitHub's label names are: a repo
    /// where a human created `Agent:Exhausted` by hand has *that* name
    /// returned by `gh issue view --json labels`, and reading it as a foreign
    /// label would make the stop sign invisible — the issue would come back as
    /// [`DispatchEligibility::NotOptedIn`] and `plan_exclusive` would add a
    /// second, differently-cased copy beside it rather than recognizing it.
    pub fn from_label_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "agent:ready" => Some(AgentLabel::Ready),
            "agent:blocked" => Some(AgentLabel::Blocked),
            "agent:exhausted" => Some(AgentLabel::Exhausted),
            _ => None,
        }
    }

    /// The description written when the label is created, so a human
    /// encountering it in the GitHub UI learns its resume semantics without
    /// reading the spec.
    pub fn description(self) -> &'static str {
        match self {
            AgentLabel::Ready => "Autopilot: opted in, eligible for dispatch",
            AgentLabel::Blocked => "Autopilot: awaiting a human; resumes on a newer human comment",
            AgentLabel::Exhausted => {
                "Autopilot: attempt cap reached; never self-resumes, a human must re-label"
            }
        }
    }

    /// Hex colour (no leading `#`, which is what `gh label create` wants).
    /// Green/amber/red, tracking how much human attention the state needs.
    pub fn color(self) -> &'static str {
        match self {
            AgentLabel::Ready => "0e8a16",
            AgentLabel::Blocked => "fbca04",
            AgentLabel::Exhausted => "b60205",
        }
    }

    /// Whether an issue in this state can start flowing again without a
    /// human re-labeling it. The spec's central distinction between the two
    /// blocked states, encoded once so no caller has to remember it.
    pub fn self_resumes(self) -> bool {
        match self {
            AgentLabel::Ready => true,
            AgentLabel::Blocked => true,
            AgentLabel::Exhausted => false,
        }
    }
}

/// Whether an issue's labels permit dispatching it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "eligibility", rename_all = "snake_case")]
pub enum DispatchEligibility {
    /// Carries `agent:ready` and nothing that overrides it.
    Eligible,
    /// Carries no `agent:*` label at all. "Does not touch unlabeled issues.
    /// An issue is invisible until explicitly opted in."
    NotOptedIn,
    /// Carries `agent:blocked`: waiting on a human answer. Auto-resumes, but
    /// not by being dispatched — the Lead flips it back to `agent:ready`
    /// first, on seeing a newer human comment.
    Blocked,
    /// Carries `agent:exhausted`. Never self-resumes.
    Exhausted,
}

impl DispatchEligibility {
    pub fn is_eligible(self) -> bool {
        matches!(self, DispatchEligibility::Eligible)
    }
}

/// Decide whether `labels` permit a dispatch.
///
/// # Precedence
///
/// The two stop states beat `agent:ready`, and `exhausted` beats `blocked`.
/// This matters because [`plan_exclusive`] can only make labels exclusive
/// *going forward* — an issue a human hand-labeled with both, or one left
/// inconsistent by a crash between the add and the remove, still has to
/// produce an answer. Resolving toward the more restrictive state is the
/// only direction that cannot start work the human meant to stop.
pub fn eligibility(labels: &[String]) -> DispatchEligibility {
    let mut has_ready = false;
    let mut has_blocked = false;
    for label in labels {
        match AgentLabel::from_label_str(label) {
            Some(AgentLabel::Exhausted) => return DispatchEligibility::Exhausted,
            Some(AgentLabel::Blocked) => has_blocked = true,
            Some(AgentLabel::Ready) => has_ready = true,
            None => {}
        }
    }
    if has_blocked {
        DispatchEligibility::Blocked
    } else if has_ready {
        DispatchEligibility::Eligible
    } else {
        DispatchEligibility::NotOptedIn
    }
}

/// The label edits needed to move an issue to exactly one `agent:*` state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LabelPlan {
    /// The target label, if it is not already present. At most one.
    pub add: Vec<String>,
    /// Every *other* `agent:*` label the issue currently carries.
    pub remove: Vec<String>,
}

impl LabelPlan {
    /// Whether applying this plan would change anything. A no-op plan is not
    /// sent to GitHub at all: `gh issue edit` with neither `--add-label` nor
    /// `--remove-label` is an error, and a redundant write would show up in
    /// the issue's timeline as if something had happened.
    pub fn is_noop(&self) -> bool {
        self.add.is_empty() && self.remove.is_empty()
    }
}

/// Plan the exclusive transition to `target` (or to *no* `agent:*` label,
/// when `target` is `None`) from the issue's `current` labels.
///
/// Only labels in [`AgentLabel::ALL`] are ever touched. Labels outside the
/// vocabulary — including unrecognized `agent:*` names — are left exactly as
/// they are: this module's authority is its own three labels, and silently
/// stripping a human's or another tool's label would exceed it.
pub fn plan_exclusive(current: &[String], target: Option<AgentLabel>) -> LabelPlan {
    let mut remove: Vec<String> = Vec::new();
    let mut target_present = false;
    for label in current {
        let Some(known) = AgentLabel::from_label_str(label) else {
            continue;
        };
        if Some(known) == target {
            target_present = true;
        } else if !remove.iter().any(|r| r == label) {
            // Removed **as spelled on the issue**, not in canonical form.
            // `from_label_str` matches case-insensitively, so a hand-created
            // `Agent:Ready` is recognized here — but whether GitHub would
            // then honour `--remove-label agent:ready` against it depends on
            // the API matching label names case-insensitively, which nothing
            // in this codebase has verified. Sending back the exact string
            // GitHub just gave us needs no such assumption. A label that
            // appears twice in different cases is therefore removed twice,
            // which is correct: they are two distinct strings to remove.
            //
            // De-duplicated because GitHub's API tolerates a repeated label
            // in the list but `gh` would send it twice.
            remove.push(label.clone());
        }
    }
    let add = match target {
        Some(label) if !target_present => vec![label.as_str().to_string()],
        _ => Vec::new(),
    };
    LabelPlan { add, remove }
}

/// What [`ensure_labels`] did for one label.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EnsuredLabel {
    /// The label did not exist and was created.
    Created,
    /// The label already existed; left untouched.
    AlreadyPresent,
}

/// Create any of the three `agent:*` labels that the repo does not already
/// have.
///
/// Idempotent by construction: `gh label create` on an existing label exits
/// non-zero with a message naming the conflict, which is treated as
/// [`EnsuredLabel::AlreadyPresent`] rather than an error. Deliberately does
/// **not** `--force` an existing label back to this module's colour and
/// description: a repo that has customized them has said something, and
/// overwriting it on every run would be this module exceeding its authority
/// for a purely cosmetic reason.
pub fn ensure_labels(
    gh: &mut dyn GhRunner,
    repo: &str,
) -> Result<Vec<(AgentLabel, EnsuredLabel)>, MemoryError> {
    validate_repo(repo)?;
    let mut results = Vec::with_capacity(AgentLabel::ALL.len());
    for label in AgentLabel::ALL {
        let argv = super::gh::label_create_argv(repo, label);
        let out = gh.run(&argv)?;
        if out.success {
            results.push((label, EnsuredLabel::Created));
            continue;
        }
        let haystack = format!("{} {}", out.stdout, out.stderr).to_lowercase();
        if LABEL_ALREADY_EXISTS_MARKERS
            .iter()
            .any(|marker| haystack.contains(marker))
        {
            results.push((label, EnsuredLabel::AlreadyPresent));
            continue;
        }
        return Err(MemoryError::Validation(format!(
            "gh label create {} failed on {repo} (exit {:?}): {}",
            label.as_str(),
            out.code,
            out.stderr.trim()
        )));
    }
    Ok(results)
}

/// Read an issue's current labels, then move it to exactly one `agent:*`
/// state (or none). Returns the plan that was applied — a no-op plan means
/// the issue was already in the target state and nothing was sent.
pub fn set_exclusive_label(
    gh: &mut dyn GhRunner,
    issue: &IssueRef,
    target: Option<AgentLabel>,
) -> Result<LabelPlan, MemoryError> {
    validate_repo(&issue.repo)?;
    let current = super::gh::issue_labels(gh, issue)?;
    let plan = plan_exclusive(&current, target);
    if plan.is_noop() {
        return Ok(plan);
    }
    let argv = super::gh::issue_edit_labels_argv(issue, &plan.add, &plan.remove);
    let out = gh.run(&argv)?;
    if !out.success {
        return Err(MemoryError::Validation(format!(
            "gh issue edit failed for {} (exit {:?}): {}",
            issue.canonical(),
            out.code,
            out.stderr.trim()
        )));
    }
    Ok(plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::GhOutput;

    fn labels(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    // ── vocabulary ──────────────────────────────────────────────────────

    #[test]
    fn every_label_round_trips_through_its_github_name() {
        for label in AgentLabel::ALL {
            assert_eq!(
                AgentLabel::from_label_str(label.as_str()),
                Some(label),
                "{} must parse back to itself",
                label.as_str()
            );
            assert!(
                label.as_str().starts_with(LABEL_PREFIX),
                "{} must live in the agent: namespace",
                label.as_str()
            );
        }
    }

    #[test]
    fn an_unknown_agent_label_is_not_ours() {
        // Another tool's `agent:*` label must not be mistaken for one of the
        // three, or `plan_exclusive` would strip it.
        assert_eq!(AgentLabel::from_label_str("agent:something-else"), None);
        assert_eq!(AgentLabel::from_label_str("priority:high"), None);
        assert_eq!(AgentLabel::from_label_str(""), None);
    }

    #[test]
    fn a_differently_cased_label_is_still_ours() {
        // GitHub label names are case-insensitive for uniqueness, so a
        // hand-created `Agent:Exhausted` is the same stop sign as
        // `agent:exhausted` and must not read as a foreign label.
        assert_eq!(
            AgentLabel::from_label_str("Agent:Exhausted"),
            Some(AgentLabel::Exhausted)
        );
        assert_eq!(
            eligibility(&labels(&["AGENT:READY"])),
            DispatchEligibility::Eligible
        );
        let plan = plan_exclusive(&labels(&["Agent:Ready"]), Some(AgentLabel::Exhausted));
        assert_eq!(
            plan.remove,
            labels(&["Agent:Ready"]),
            "removed as spelled on the issue, so removal needs no assumption \
about GitHub matching label names case-insensitively"
        );
        assert_eq!(
            plan.add,
            labels(&["agent:exhausted"]),
            "but the label we create is always the canonical spelling"
        );
    }

    #[test]
    fn only_exhausted_refuses_to_self_resume() {
        // The spec's whole reason for having two blocked states.
        assert!(!AgentLabel::Exhausted.self_resumes());
        assert!(AgentLabel::Blocked.self_resumes());
        assert!(AgentLabel::Ready.self_resumes());
    }

    // ── eligibility ─────────────────────────────────────────────────────

    #[test]
    fn an_unlabeled_issue_is_invisible() {
        assert_eq!(
            eligibility(&labels(&["bug", "priority:high"])),
            DispatchEligibility::NotOptedIn
        );
        assert_eq!(eligibility(&[]), DispatchEligibility::NotOptedIn);
    }

    #[test]
    fn agent_ready_alone_is_eligible() {
        assert_eq!(
            eligibility(&labels(&["agent:ready", "bug"])),
            DispatchEligibility::Eligible
        );
    }

    #[test]
    fn exhausted_beats_ready_so_a_stopped_issue_never_restarts() {
        // The livelock the spec's cross-dispatch stagnation control exists to
        // prevent: if a stale `agent:ready` could outvote `agent:exhausted`,
        // the daily budget reset would re-pick the issue tomorrow.
        assert_eq!(
            eligibility(&labels(&["agent:ready", "agent:exhausted"])),
            DispatchEligibility::Exhausted
        );
        assert_eq!(
            eligibility(&labels(&["agent:exhausted", "agent:ready"])),
            DispatchEligibility::Exhausted,
            "order in the label list must not change the answer"
        );
    }

    #[test]
    fn exhausted_beats_blocked_too() {
        assert_eq!(
            eligibility(&labels(&["agent:blocked", "agent:exhausted"])),
            DispatchEligibility::Exhausted
        );
    }

    #[test]
    fn blocked_beats_ready() {
        assert_eq!(
            eligibility(&labels(&["agent:ready", "agent:blocked"])),
            DispatchEligibility::Blocked
        );
    }

    // ── transitions ─────────────────────────────────────────────────────

    #[test]
    fn moving_to_a_label_removes_the_others() {
        let plan = plan_exclusive(
            &labels(&["agent:ready", "agent:blocked", "bug"]),
            Some(AgentLabel::Exhausted),
        );
        assert_eq!(plan.add, labels(&["agent:exhausted"]));
        assert_eq!(plan.remove, labels(&["agent:ready", "agent:blocked"]));
    }

    #[test]
    fn a_label_already_in_the_target_state_is_a_noop() {
        let plan = plan_exclusive(&labels(&["agent:ready"]), Some(AgentLabel::Ready));
        assert!(
            plan.is_noop(),
            "nothing to do, so nothing is sent: {plan:?}"
        );
    }

    #[test]
    fn clearing_removes_every_agent_label_and_adds_none() {
        // The merge path: a merged issue must carry no `agent:*` label, or a
        // stale `agent:ready` makes the Lead re-pick finished work forever.
        let plan = plan_exclusive(
            &labels(&["agent:ready", "agent:blocked", "agent:exhausted"]),
            None,
        );
        assert!(plan.add.is_empty());
        assert_eq!(
            plan.remove,
            labels(&["agent:ready", "agent:blocked", "agent:exhausted"])
        );
    }

    #[test]
    fn foreign_labels_are_never_touched() {
        let plan = plan_exclusive(
            &labels(&["bug", "priority:high", "agent:custom", "agent:ready"]),
            Some(AgentLabel::Blocked),
        );
        assert_eq!(plan.add, labels(&["agent:blocked"]));
        assert_eq!(
            plan.remove,
            labels(&["agent:ready"]),
            "only this module's own vocabulary is in scope"
        );
    }

    #[test]
    fn two_spellings_of_one_label_are_both_removed() {
        // They are two distinct strings on the issue, so both must be sent.
        let plan = plan_exclusive(
            &labels(&["agent:ready", "Agent:Ready"]),
            Some(AgentLabel::Exhausted),
        );
        assert_eq!(plan.remove, labels(&["agent:ready", "Agent:Ready"]));
    }

    #[test]
    fn a_duplicated_label_is_removed_once() {
        let plan = plan_exclusive(
            &labels(&["agent:ready", "agent:ready"]),
            Some(AgentLabel::Exhausted),
        );
        assert_eq!(plan.remove, labels(&["agent:ready"]));
    }

    // ── ensure_labels ───────────────────────────────────────────────────

    #[test]
    fn ensure_labels_creates_all_three() {
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok("")),
        ]);
        let results = ensure_labels(&mut gh, "owner/repo").unwrap();
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|(_, e)| *e == EnsuredLabel::Created));
        assert_eq!(gh.seen.len(), 3);
        assert!(gh.seen[0].contains(&"label".to_string()));
    }

    #[test]
    fn an_already_existing_label_is_not_an_error() {
        // `gh label create` exits non-zero on a name that already exists.
        // Treating that as failure would make `ensure_labels` usable exactly
        // once per repo.
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::failed(
                "",
                "HTTP 422: Validation Failed (label already exists)",
            )),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::failed("", "label with this name already exists")),
        ]);
        let results = ensure_labels(&mut gh, "owner/repo").unwrap();
        assert_eq!(
            results
                .iter()
                .filter(|(_, e)| *e == EnsuredLabel::AlreadyPresent)
                .count(),
            2
        );
    }

    #[test]
    fn a_real_label_failure_is_an_error() {
        // Anything that is not the already-exists case must surface: a repo
        // we cannot write labels to is a repo whose issues we cannot mark
        // exhausted, and silently continuing would lose the stop signal.
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::failed("", "HTTP 403: Forbidden"))]);
        let err = ensure_labels(&mut gh, "owner/repo").unwrap_err();
        assert!(
            err.to_string().contains("403"),
            "the operator needs the real reason: {err}"
        );
    }

    #[test]
    fn ensure_labels_validates_the_repo_before_spawning_anything() {
        let mut gh = ScriptedGh::new(vec![]);
        assert!(ensure_labels(&mut gh, "   ").is_err());
        assert!(gh.seen.is_empty(), "nothing should have been run");
    }

    // ── set_exclusive_label ─────────────────────────────────────────────

    #[test]
    fn setting_a_label_reads_current_state_then_edits() {
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
        ]);
        let plan = set_exclusive_label(
            &mut gh,
            &IssueRef::new("owner/repo", 7),
            Some(AgentLabel::Exhausted),
        )
        .unwrap();
        assert_eq!(plan.add, labels(&["agent:exhausted"]));
        assert_eq!(plan.remove, labels(&["agent:ready"]));
        assert_eq!(gh.seen.len(), 2, "one read, one write");
        let edit = &gh.seen[1];
        assert!(edit.contains(&"--add-label".to_string()));
        assert!(edit.contains(&"--remove-label".to_string()));
    }

    #[test]
    fn a_noop_transition_sends_no_edit_at_all() {
        // `gh issue edit` with neither --add-label nor --remove-label is an
        // error, and a redundant edit would show in the issue timeline as if
        // something had changed.
        let mut gh = ScriptedGh::new(vec![Ok(GhOutput::ok(
            r#"{"labels":[{"name":"agent:ready"}]}"#,
        ))]);
        let plan = set_exclusive_label(
            &mut gh,
            &IssueRef::new("owner/repo", 7),
            Some(AgentLabel::Ready),
        )
        .unwrap();
        assert!(plan.is_noop());
        assert_eq!(gh.seen.len(), 1, "only the read happened");
    }

    #[test]
    fn a_failed_edit_is_an_error_not_a_silent_success() {
        let mut gh = ScriptedGh::new(vec![
            Ok(GhOutput::ok(r#"{"labels":[]}"#)),
            Ok(GhOutput::failed("", "HTTP 403: Forbidden")),
        ]);
        let err = set_exclusive_label(
            &mut gh,
            &IssueRef::new("owner/repo", 7),
            Some(AgentLabel::Exhausted),
        )
        .unwrap_err();
        assert!(err.to_string().contains("403"));
    }
}
