//! The turn-prompt template — build-ladder rung 2.
//!
//! Renders the `/goal` condition text the spec's *The turn-prompt template*
//! section defines and rung 0 measured against a real gate. This module is
//! pure text construction: it takes already-loaded lineage/gate-config data
//! (rung 1's storage layer) and produces the exact string that fills the
//! `"..."` in `claude -p "/goal <condition> or stop after N turns"`.
//!
//! # One definition of "done" (spec open question, resolved by construction)
//!
//! The rendered condition's gate line is generated from the repo's approved
//! [`super::gate_config::GateConfig`], never authored separately — so the
//! `/goal` condition and the approved gate config cannot disagree. See
//! [`render`]'s doc and `n_equals_one_reproduces_rev_4_form` below.

use super::lineage::{AttemptOutcome, AttemptRecord};
use super::IssueRef;

/// One prior attempt, formatted for the "Prior attempts" section. Mirrors
/// [`AttemptRecord`] but flattened to what the template actually prints — a
/// dispatch runner reads these from [`super::lineage::attempts_for_issue`].
pub struct PriorAttempt {
    pub attempt_n: u32,
    pub approach: String,
    pub verdict: AttemptOutcome,
    pub why_failed: Option<String>,
}

impl From<&AttemptRecord> for PriorAttempt {
    fn from(record: &AttemptRecord) -> Self {
        Self {
            attempt_n: record.attempt_n,
            approach: record.approach.clone(),
            verdict: record.verdict,
            why_failed: record.why_failed.clone(),
        }
    }
}

/// Inputs to [`render`]. Grouped into a struct, matching
/// [`super::dispatch_state::DispatchState`]'s pattern, so the call site
/// stays readable as fields accrue.
pub struct TurnPromptInputs<'a> {
    pub issue: &'a IssueRef,
    pub issue_title: &'a str,
    /// Issue body, or a summary if it exceeds budget — the caller decides
    /// what "exceeds budget" means; this module just prints what it's given.
    pub issue_body: &'a str,
    /// Oldest-first, as returned by [`super::lineage::attempts_for_issue`].
    pub prior_attempts: &'a [PriorAttempt],
    /// A strategy redirect in force for this dispatch, if the Lead's
    /// strategy-health check fired one. `None` for a normal dispatch.
    pub strategy_redirect: Option<&'a str>,
    /// The repo's approved gate commands, verbatim from
    /// [`super::gate_config::GateConfig::gate_commands`] — never authored
    /// separately from the approved config.
    pub gate_commands: &'a [String],
    /// Turns per dispatch (the spec's **N**). Must be at least 1; see
    /// [`render`]'s panic doc.
    pub n_turns: u32,
}

/// Render the `/goal` condition text for one IC dispatch.
///
/// # Panics
///
/// Panics if `inputs.n_turns == 0` — "stop after 0 turns" is not a coherent
/// dispatch and every caller controls this value directly (it is never
/// parsed from untrusted input), so a panic surfaces a caller bug immediately
/// rather than silently emitting a nonsensical condition string.
///
/// Panics if `inputs.gate_commands` is empty, for the same reason: an empty
/// gate would render a vacuous condition ("...never authored separately):
/// .") that an IC could trivially call `met` against, silently defeating
/// this module's "one definition of done" guarantee.
pub fn render(inputs: &TurnPromptInputs) -> String {
    assert!(
        inputs.n_turns >= 1,
        "n_turns must be at least 1, got {}",
        inputs.n_turns
    );
    assert!(
        !inputs.gate_commands.is_empty(),
        "{}",
        super::EMPTY_GATE_COMMANDS_MSG
    );

    let lineage_section = if inputs.prior_attempts.is_empty() {
        "none yet".to_string()
    } else {
        inputs
            .prior_attempts
            .iter()
            .map(format_prior_attempt)
            .collect::<Vec<_>>()
            .join("\n")
    };

    let redirect_line = inputs
        .strategy_redirect
        .map(|r| format!("\n{r}"))
        .unwrap_or_default();

    let gate_line = inputs.gate_commands.join(" && ");

    format!(
        "You are an IC dispatch for issue {issue}: \"{title}\".\n\n\
{body}\n\n\
Prior attempts on this issue (read before doing anything else):\n\
{lineage}{redirect}\n\n\
Constraints: feature-branch push only, never push to the default branch. Stay \
inside your worktree. Never touch credential or secret files.\n\n\
Checkpoint your progress (what you tried, current state, next step) after \
EVERY turn, not just at the end — you may be re-invoked as a fresh process \
with only this checkpoint and the transcript to resume from.\n\n\
The gate condition for this repo (generated from its approved gate config, \
never authored separately): {gate}.\n\n\
Report your verdict using the required output schema when, and only when, \
you have either satisfied the gate condition above or determined it cannot be \
satisfied. Do not guess; if you are unsure whether it is met, the verdict is \
not_met and you take another turn.\n\n\
or stop after {n} turns",
        issue = inputs.issue.canonical(),
        title = inputs.issue_title,
        body = inputs.issue_body,
        lineage = lineage_section,
        redirect = redirect_line,
        gate = gate_line,
        n = inputs.n_turns,
    )
}

fn format_prior_attempt(attempt: &PriorAttempt) -> String {
    let verdict = match attempt.verdict {
        AttemptOutcome::Success => "success",
        AttemptOutcome::Failed => "failed",
    };
    let why = attempt
        .why_failed
        .as_deref()
        .map(|w| format!(" ({w})"))
        .unwrap_or_default();
    format!(
        "attempt {n}: {approach} — {verdict}{why}",
        n = attempt.attempt_n,
        approach = attempt.approach,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_issue() -> IssueRef {
        IssueRef::new("ironrace/ironmem", 283)
    }

    #[test]
    fn first_attempt_has_no_prior_attempts_section() {
        let issue = base_issue();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "Add harness support",
            issue_body: "Body text.",
            prior_attempts: &[],
            strategy_redirect: None,
            gate_commands: &["cargo test --workspace".to_string()],
            n_turns: 6,
        });
        assert!(text
            .contains("Prior attempts on this issue (read before doing anything else):\nnone yet"));
    }

    #[test]
    fn prior_attempts_render_one_line_each_with_reason() {
        let issue = base_issue();
        let prior = vec![
            PriorAttempt {
                attempt_n: 1,
                approach: "tried approach A".into(),
                verdict: AttemptOutcome::Failed,
                why_failed: Some("test X failed".into()),
            },
            PriorAttempt {
                attempt_n: 2,
                approach: "tried approach B".into(),
                verdict: AttemptOutcome::Success,
                why_failed: None,
            },
        ];
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &prior,
            strategy_redirect: None,
            gate_commands: &["cargo test".to_string()],
            n_turns: 1,
        });
        assert!(text.contains("attempt 1: tried approach A — failed (test X failed)"));
        assert!(text.contains("attempt 2: tried approach B — success"));
        assert!(!text.contains("none yet"));
    }

    #[test]
    fn strategy_redirect_is_stated_explicitly_when_present() {
        let issue = base_issue();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: Some(
                "Do not retry approach A; it failed for reason Y. Try approach C instead.",
            ),
            gate_commands: &["cargo test".to_string()],
            n_turns: 3,
        });
        assert!(text
            .contains("Do not retry approach A; it failed for reason Y. Try approach C instead."));
    }

    #[test]
    fn gate_condition_is_generated_from_the_approved_config_verbatim() {
        // Spec's "one definition of done": the gate line must be built from
        // gate_commands, not authored separately — this test pins that the
        // rendered text contains exactly the joined gate commands, not a
        // paraphrase of them.
        let issue = base_issue();
        let commands = vec![
            "cargo fmt --all -- --check".to_string(),
            "cargo test --workspace".to_string(),
        ];
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            gate_commands: &commands,
            n_turns: 1,
        });
        assert!(text.contains("cargo fmt --all -- --check && cargo test --workspace"));
    }

    #[test]
    fn n_equals_one_reproduces_rev_4_form() {
        // rev 5's parameterisation must generalise rev 4, not replace it —
        // N = 1 renders "or stop after 1 turns", the same clause rev 4's
        // single-turn dispatch always implied.
        let issue = base_issue();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            gate_commands: &["cargo test".to_string()],
            n_turns: 1,
        });
        assert!(text.ends_with("or stop after 1 turns"));
    }

    #[test]
    #[should_panic(expected = "n_turns must be at least 1")]
    fn zero_turns_panics() {
        let issue = base_issue();
        render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            gate_commands: &["cargo test".to_string()],
            n_turns: 0,
        });
    }

    #[test]
    #[should_panic(expected = "gate_commands must not be empty")]
    fn empty_gate_commands_panics() {
        // An empty gate would render a vacuous "/goal" condition an IC could
        // trivially call `met` against — must fail as loudly as n_turns == 0.
        let issue = base_issue();
        render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            gate_commands: &[],
            n_turns: 1,
        });
    }

    #[test]
    fn condition_never_exceeds_the_documented_4000_char_limit_for_a_realistic_case() {
        // ⟨r5-doc⟩: the condition may be up to 4,000 characters. Not a hard
        // guard here (issue bodies are the caller's to bound), but a
        // regression check that a realistic dispatch (a handful of prior
        // attempts, one gate command) stays comfortably under it.
        let issue = base_issue();
        let prior: Vec<PriorAttempt> = (1..=5)
            .map(|n| PriorAttempt {
                attempt_n: n,
                approach: "a reasonably descriptive approach summary".into(),
                verdict: AttemptOutcome::Failed,
                why_failed: Some("a reasonably descriptive failure reason".into()),
            })
            .collect();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "A realistic issue title",
            issue_body: "A realistic issue body of a few sentences describing the work.",
            prior_attempts: &prior,
            strategy_redirect: None,
            gate_commands: &["cargo test --workspace".to_string()],
            n_turns: 6,
        });
        assert!(text.chars().count() < 4_000);
    }
}
