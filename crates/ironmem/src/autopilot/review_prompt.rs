//! The Reviewer prompt template — build-ladder rung 5.
//!
//! Renders the single prompt string handed to the fresh-context, read-only
//! Reviewer the spec's *Roles* section defines. Pure text construction, in
//! the same shape as rung 2's [`super::turn_prompt`]: it takes already-loaded
//! data (the issue, the PR, the dispatch-time class, the approved gate
//! commands) and produces the exact string that becomes `codex exec
//! <prompt>`.
//!
//! # Why the prompt states both jobs explicitly
//!
//! The spec gives the Reviewer **two** merge-time jobs — re-classify the
//! diff's risk, *and* review it for correctness/security — and treats them
//! as equally load-bearing: a PASS on a diff whose class the Reviewer got
//! wrong still fails closed at [`super::review::decide_merge`], but only if the
//! Reviewer actually reported a class derived from the diff rather than
//! echoing the one it was told. The prompt therefore names the dispatch-time
//! class as *the Lead's expectation to be checked*, never as the answer, and
//! says out loud that agreeing without looking defeats the check.
//!
//! # Why "read-only" is stated as well as sandboxed
//!
//! [`super::review::build_argv`] passes `codex exec -s read-only`, so the sandbox
//! already refuses writes. The prompt repeats the constraint because a
//! sandbox denial surfaces to the model as a *tool failure* mid-review —
//! something it may burn turns retrying or route around — whereas an
//! instruction it read up front stops it attempting the write at all.

use super::IssueRef;

/// Inputs to [`render`]. Grouped into a struct, matching
/// [`super::turn_prompt::TurnPromptInputs`]'s pattern.
pub struct ReviewPromptInputs<'a> {
    pub issue: &'a IssueRef,
    /// The pull request the IC opened for this issue.
    pub pr_number: u64,
    /// The base branch the PR merges into, so the Reviewer diffs against the
    /// right thing rather than guessing `main`.
    pub base_branch: &'a str,
    /// The head branch the IC pushed (`autopilot/<slug>-<n>`).
    pub head_branch: &'a str,
    /// The Lead's dispatch-time risk class, verbatim. Presented to the
    /// Reviewer as an expectation to check against the real diff, never as
    /// the answer — see the module doc.
    pub dispatch_class: &'a str,
    /// The repo's approved gate commands, verbatim from
    /// [`super::gate_config::GateConfig::gate_commands`]. The Reviewer
    /// does not *run* these (it is read-only); it is told what "green" meant
    /// so it can judge whether the diff's tests actually exercise the change.
    pub gate_commands: &'a [String],
}

/// Render the Reviewer's prompt.
///
/// # Panics
///
/// Panics if `inputs.gate_commands` is empty, for the same reason
/// [`super::turn_prompt::render`] does: a Reviewer told nothing about what
/// "green" meant for this repo cannot judge whether the diff's tests exercise
/// the change, and every caller controls this value directly (it comes from
/// an *approved* gate config, which [`super::gate_config::propose_gate_config`]
/// already refuses to create empty).
pub fn render(inputs: &ReviewPromptInputs) -> String {
    assert!(
        !inputs.gate_commands.is_empty(),
        "{}",
        super::EMPTY_GATE_COMMANDS_MSG
    );

    let gate_line = inputs.gate_commands.join(" && ");

    format!(
        "You are a fresh-context, read-only reviewer for pull request #{pr} on \
{repo}, which was opened by an autonomous agent to close issue {issue}.\n\n\
Read the diff first: `git diff {base}...{head}` (or `gh pr diff {pr}`). Review \
the whole diff, not a sample of it.\n\n\
You have TWO jobs, and both must be answered from the diff you just read:\n\n\
1. CLASSIFY the actual change. Choose exactly one risk class:\n\
   - documentation — documentation/comment text only\n\
   - dependency_bump — dependency version changes only\n\
   - mechanical_rename — a rename with no behavior change anywhere\n\
   - test_only — changes confined to test code\n\
   - logic — any change to runtime behavior\n\
   - protocol — any change to a wire format, schema, or message contract\n\
   - security — anything touching auth, secrets, permissions, or sandboxing\n\
   - public_api — any change to an exported/public interface\n\
   The Lead classified this issue as \"{dispatch_class}\" BEFORE the code \
existed. That is an expectation to check, not the answer. Classify what the \
diff actually does; if the two disagree, say so by reporting the class you \
derived — a disagreement is a useful signal and is handled downstream, so \
there is no reason to soften it. If more than one class fits, report the \
most severe one.\n\n\
2. REVIEW the diff for correctness and security. Return `pass` only if you \
would be comfortable with this merging unread by a human. Return \
`needs_changes` for any real defect, and also whenever you are uncertain \
— uncertainty is not a pass. Say specifically what is wrong and where.\n\n\
The repo's gate (already run and green before you were dispatched) is: \
{gate}. You are NOT re-running it — judge instead whether the diff's own \
tests actually exercise the change, and whether a green gate could be hiding \
a defect here.\n\n\
Constraints: you are read-only. Do not edit files, do not commit, do not \
push, do not comment on the PR, and do not merge. You hold no state and \
supervise nothing; this is a single review pass.\n\n\
Report your verdict and your risk class using the required output schema.",
        pr = inputs.pr_number,
        repo = inputs.issue.repo,
        issue = inputs.issue.canonical(),
        base = inputs.base_branch,
        head = inputs.head_branch,
        dispatch_class = inputs.dispatch_class,
        gate = gate_line,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_inputs<'a>(issue: &'a IssueRef, gates: &'a [String]) -> ReviewPromptInputs<'a> {
        ReviewPromptInputs {
            issue,
            pr_number: 322,
            base_branch: "main",
            head_branch: "autopilot/ironrace-ironmem-283",
            dispatch_class: "documentation",
            gate_commands: gates,
        }
    }

    #[test]
    fn names_both_jobs_and_every_risk_class() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test --workspace".to_string()];
        let prompt = render(&sample_inputs(&issue, &gates));

        assert!(prompt.contains("CLASSIFY"));
        assert!(prompt.contains("REVIEW"));
        for class in [
            "documentation",
            "dependency_bump",
            "mechanical_rename",
            "test_only",
            "logic",
            "protocol",
            "security",
            "public_api",
        ] {
            assert!(prompt.contains(class), "prompt omits risk class {class}");
        }
    }

    #[test]
    fn frames_the_dispatch_class_as_an_expectation_not_the_answer() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test --workspace".to_string()];
        let prompt = render(&sample_inputs(&issue, &gates));
        assert!(prompt.contains("an expectation to check, not the answer"));
    }

    #[test]
    fn uncertainty_is_explicitly_not_a_pass() {
        // The spec's error table routes "reviewer uncertain" to the same
        // place as NEEDS CHANGES. `decide_merge` can only honor that if the
        // Reviewer was told to report uncertainty as `needs_changes` rather
        // than resolving it into a pass on its own.
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test --workspace".to_string()];
        let prompt = render(&sample_inputs(&issue, &gates));
        assert!(prompt.contains("uncertainty is not a pass"));
    }

    #[test]
    fn states_the_read_only_constraints() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test --workspace".to_string()];
        let prompt = render(&sample_inputs(&issue, &gates));
        assert!(prompt.contains("read-only"));
        assert!(prompt.contains("do not merge"));
        assert!(prompt.contains("do not push"));
    }

    #[test]
    fn gate_commands_are_joined_and_marked_not_re_run() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string(), "cargo clippy".to_string()];
        let prompt = render(&sample_inputs(&issue, &gates));
        assert!(prompt.contains("cargo test && cargo clippy"));
        assert!(prompt.contains("NOT re-running it"));
    }

    #[test]
    fn diff_range_uses_the_supplied_base_and_head() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates = vec!["cargo test".to_string()];
        let mut inputs = sample_inputs(&issue, &gates);
        inputs.base_branch = "develop";
        inputs.head_branch = "autopilot/x-1";
        let prompt = render(&inputs);
        assert!(prompt.contains("git diff develop...autopilot/x-1"));
    }

    #[test]
    #[should_panic(expected = "gate_commands must not be empty")]
    fn empty_gate_commands_panic() {
        let issue = IssueRef::new("ironrace/ironmem", 283);
        let gates: Vec<String> = Vec::new();
        let _ = render(&sample_inputs(&issue, &gates));
    }
}
