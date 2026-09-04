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

/// What a remediation dispatch is being asked to fix.
///
/// The mechanical half (which PR, which commit, and the instruction that the
/// fix must be *pushed*) is separate from the optional half (what the reviewer
/// actually said) so the two can be composed on read — rung 9's lesson 35. A
/// reviewer that returns `needs_changes` with no reason still produces a
/// coherent instruction; joining them at write time would make the guaranteed
/// half loseable with the optional one.
#[derive(Debug, Clone, Copy)]
pub struct RemediationBrief<'a> {
    pub pr_number: u64,
    /// The commit the reviewer read — the one the head must move *past*.
    pub head_sha: &'a str,
    /// The reviewer's recorded reason, if it gave one.
    pub findings: Option<&'a str>,
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
    /// Questions Autopilot posted on the issue that a human has since
    /// answered, oldest first — rung 8's
    /// [`super::blocked::active_answers`].
    ///
    /// This is the delivery half of the spec's *"appends the answer to
    /// lineage, flips back to `agent:ready`, re-dispatches"*. Without it the
    /// re-dispatch would resume a session that asked a question and was never
    /// told the answer, and the IC's only rational move would be to ask it
    /// again — a loop between two halves of the same mechanism.
    pub human_answers: &'a [(String, String)],
    /// A reviewer's `needs_changes` findings this dispatch exists to address
    /// — rung 11's [`super::remediate::active_remediation`]. `None` for an
    /// ordinary dispatch.
    ///
    /// **This changes the rendered goal condition, not just the prose.** A
    /// remediation dispatch re-opens work whose gate is *already green* at the
    /// reviewed commit, so the ordinary condition would be satisfied the
    /// instant the IC ran the gate: it would report `met`, push nothing, and
    /// the next review would read the identical commit and say the identical
    /// thing. See [`render`]'s remediation clause.
    pub remediation: Option<RemediationBrief<'a>>,
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
///
/// # The remediation clause
///
/// [`TurnPromptInputs::remediation`] does something no other input here does:
/// it **extends the goal condition**. Every other field adds context the IC
/// reads on its way to the same target. A remediation dispatch's target is
/// different, because it re-opens an issue whose gate is already green — so
/// rendering the ordinary condition would hand the IC a goal it satisfies by
/// doing nothing, and "the gate passes" would authorize a dispatch that pushed
/// no commit at all. With a remediation in force the condition is the gate
/// **and** a pushed commit addressing the findings, and the section above it
/// says why in the IC's own terms. The two halves are composed here rather
/// than stored joined, so a reviewer that gave no reason still gets a coherent
/// instruction.
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

    // Rendered before the constraints rather than appended to the lineage
    // block: an answer is a human *instruction* for this dispatch, not a
    // record of a past attempt, and burying it inside a list of failures is
    // how it gets skimmed past.
    let answers_section = if inputs.human_answers.is_empty() {
        String::new()
    } else {
        let mut section = String::from(
            "\n\nA human answered the question(s) you asked on this issue. \
These answers are decisions, not suggestions — follow them:\n",
        );
        for (question, answer) in inputs.human_answers {
            section.push_str(&format!(
                "- you asked: {question}\n  the answer: {answer}\n"
            ));
        }
        section
    };

    // Rendered last of the three context blocks and immediately before the
    // constraints, because it is the only one that changes what "done" means
    // for this dispatch. The mechanical instruction is emitted whether or not
    // the reviewer gave a reason: the verdict alone is actionable, and a
    // remediation that rendered nothing without findings would silently become
    // an ordinary dispatch against an already-green gate.
    let remediation_section = match &inputs.remediation {
        None => String::new(),
        Some(brief) => {
            let findings = match brief.findings.map(str::trim).filter(|f| !f.is_empty()) {
                Some(findings) => format!("\n\nThe reviewer said:\n{findings}"),
                None => "\n\nThe reviewer recorded no reason with its verdict. Re-read the diff on this branch as a hostile reviewer would and fix what it would object to. If you genuinely find nothing to change, say so in your checkpoint — do not push an empty commit to move the head."
                    .to_string(),
            };
            format!(
                "\n\nA reviewer read pull request #{pr} at commit {sha} and asked for CHANGES. This dispatch exists to address that review. It is not a fresh start on the issue: the work already on the branch is yours to fix, not to redo.\n\nThis branch has ALREADY MET the gate below once — that is why there is a pull request to review — so running the gate and watching it pass does not mean you are done. You are done only when the findings are addressed, the gate passes, and you have PUSHED the result to this issue's branch. A dispatch that reports the gate met without pushing a new commit has changed nothing — the reviewer will read commit {sha} again and return the same verdict.{findings}",
                pr = brief.pr_number,
                sha = brief.head_sha,
            )
        }
    };

    // The condition itself, not just the prose around it. See the doc above.
    let gate_extra = if inputs.remediation.is_some() {
        ", and every finding in the review above is addressed by a commit you have pushed to this branch"
    } else {
        ""
    };

    let gate_line = inputs.gate_commands.join(" && ");

    format!(
        "You are an IC dispatch for issue {issue}: \"{title}\".\n\n\
{body}\n\n\
Prior attempts on this issue (read before doing anything else):\n\
{lineage}{redirect}{answers}{remediation}\n\n\
Constraints: feature-branch push only, never push to the default branch. Stay \
inside your worktree. Never touch credential or secret files.\n\n\
Checkpoint your progress (what you tried, current state, next step) after \
EVERY turn, not just at the end — you may be re-invoked as a fresh process \
with only this checkpoint and the transcript to resume from.\n\n\
The gate condition for this repo (generated from its approved gate config, \
never authored separately): {gate}{gate_extra}.\n\n\
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
        answers = answers_section,
        remediation = remediation_section,
        gate = gate_line,
        gate_extra = gate_extra,
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

    fn remediation_text(findings: Option<&str>) -> String {
        let issue = base_issue();
        render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            human_answers: &[],
            remediation: Some(RemediationBrief {
                pr_number: 42,
                head_sha: "deadbeef",
                findings,
            }),
            gate_commands: &["cargo test".to_string()],
            n_turns: 6,
        })
    }

    #[test]
    fn a_realistic_remediation_condition_stays_under_the_4000_char_platform_limit() {
        // ⟨r5-doc⟩'s 4,000 characters is a `/goal` limit, not a style
        // preference, and rung 11 is the first thing to add a *large* block to
        // the condition. Measured: this template renders 1,498 characters for
        // a realistic dispatch and 2,570 with the remediation block and no
        // findings, which is where `remediate::MAX_FINDINGS_CHARS` (1,200)
        // comes from — the two together leave real headroom rather than
        // landing exactly on the limit.
        let issue = base_issue();
        let prior: Vec<PriorAttempt> = (1..=5)
            .map(|n| PriorAttempt {
                attempt_n: n,
                approach: "a reasonably descriptive approach summary".into(),
                verdict: AttemptOutcome::Failed,
                why_failed: Some("a reasonably descriptive failure reason".into()),
            })
            .collect();
        let findings = "x".repeat(super::super::remediate::MAX_FINDINGS_CHARS);
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "A realistic issue title",
            issue_body: "A realistic issue body of a few sentences describing the work.",
            prior_attempts: &prior,
            strategy_redirect: None,
            human_answers: &[],
            remediation: Some(RemediationBrief {
                pr_number: 42,
                head_sha: "deadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                findings: Some(&findings),
            }),
            gate_commands: &["cargo test --workspace".to_string()],
            n_turns: 6,
        });
        assert!(
            text.chars().count() < 4_000,
            "a remediation condition at the findings bound must still fit the \
platform limit, got {}",
            text.chars().count()
        );
    }

    #[test]
    fn a_remediation_extends_the_goal_condition_beyond_the_gate() {
        // THE load-bearing property of rung 11. The gate is already green at
        // the reviewed commit, so a condition that says only "the gate passes"
        // is satisfied by doing nothing: the IC would run the gate, report
        // `met`, push no commit, and the next review would read the identical
        // commit and return the identical verdict — forever, at full price.
        let text = remediation_text(Some("the retry loop is unbounded"));
        let gate_at = text.find("The gate condition for this repo").unwrap();
        let condition = &text[gate_at..];
        assert!(
            condition.contains("addressed by a commit you have pushed to this branch"),
            "the pushed fix must be part of the CONDITION, not just the prose: {condition}"
        );
    }

    #[test]
    fn an_ordinary_dispatch_condition_is_left_exactly_as_it_was() {
        // Rung 9's lesson 43: a new optional feature must not change the
        // configuration that predates it.
        let issue = base_issue();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            human_answers: &[],
            remediation: None,
            gate_commands: &["cargo test".to_string()],
            n_turns: 6,
        });
        assert!(text.contains("never authored separately): cargo test."));
        assert!(!text.contains("asked for CHANGES"));
        assert!(!text.contains("pushed to this branch"));
    }

    #[test]
    fn a_remediation_names_the_pr_the_commit_and_the_findings() {
        let text = remediation_text(Some("the retry loop is unbounded"));
        assert!(text.contains("pull request #42"));
        assert!(text.contains("deadbeef"));
        assert!(text.contains("The reviewer said:\nthe retry loop is unbounded"));
    }

    #[test]
    fn a_remediation_says_why_passing_the_gate_is_not_the_job() {
        // The IC has to be told *why* passing the gate is not the job, or the
        // instruction reads as boilerplate it can satisfy the easy way.
        //
        // Phrased as a claim about the BRANCH, not about the reviewed commit.
        // A remediation is only ever armed on an issue whose lineage records a
        // success, so "this branch has met the gate once" is true by
        // construction — whereas "the gate is green at commit X" is false
        // whenever the head has moved past the commit the gate was green at,
        // which is exactly the `gate_green == false` case `advance` still arms
        // in. Telling an IC something false about its own repository is not
        // worth the extra force of the stronger sentence.
        let text = remediation_text(Some("f"));
        assert!(text.contains("ALREADY MET the gate"));
        assert!(
            text.contains("without pushing a new commit has changed nothing"),
            "the failure mode has to be named, not implied"
        );
    }

    #[test]
    fn a_remediation_with_no_findings_still_carries_the_mechanical_instruction() {
        // Rung 9's lesson 35: the guaranteed half is composed on read, so a
        // reviewer that returned a bare verdict cannot silently turn a
        // remediation back into an ordinary dispatch against a green gate.
        let text = remediation_text(None);
        assert!(text.contains("asked for CHANGES"));
        assert!(text.contains("ALREADY MET the gate"));
        assert!(text.contains("addressed by a commit you have pushed to this branch"));
        assert!(text.contains("recorded no reason"));
        assert!(!text.contains("The reviewer said:"));
    }

    #[test]
    fn blank_findings_read_as_no_findings() {
        let text = remediation_text(Some("   \n "));
        assert!(
            !text.contains("The reviewer said:"),
            "an all-whitespace reason must not render an empty findings block"
        );
        assert!(text.contains("recorded no reason"));
    }

    #[test]
    fn a_remediation_is_rendered_after_the_lineage_and_before_the_constraints() {
        // Same placement rule as rung 8's answers, for the same reason: an
        // instruction buried inside a list of past attempts gets skimmed. It
        // goes last of the three context blocks because it is the only one
        // that changes what "done" means.
        let issue = base_issue();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[PriorAttempt {
                attempt_n: 1,
                approach: "tried A".to_string(),
                verdict: AttemptOutcome::Success,
                why_failed: None,
            }],
            strategy_redirect: None,
            human_answers: &[("Q?".to_string(), "A!".to_string())],
            remediation: Some(RemediationBrief {
                pr_number: 42,
                head_sha: "deadbeef",
                findings: Some("fix it"),
            }),
            gate_commands: &["cargo test".to_string()],
            n_turns: 6,
        });
        let attempts_at = text.find("tried A").unwrap();
        let answers_at = text.find("A!").unwrap();
        let remediation_at = text.find("asked for CHANGES").unwrap();
        let constraints_at = text.find("Constraints:").unwrap();
        assert!(attempts_at < answers_at);
        assert!(answers_at < remediation_at);
        assert!(remediation_at < constraints_at);
    }

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
            human_answers: &[],
            remediation: None,
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
            human_answers: &[],
            remediation: None,
            gate_commands: &["cargo test".to_string()],
            n_turns: 1,
        });
        assert!(text.contains("attempt 1: tried approach A — failed (test X failed)"));
        assert!(text.contains("attempt 2: tried approach B — success"));
        assert!(!text.contains("none yet"));
    }

    #[test]
    fn an_answered_question_reaches_the_next_dispatchs_condition() {
        // Rung 8's blocked round trip is only closed if the answer is
        // *delivered*. Without this the re-dispatched IC resumes a session
        // that asked a question and was never told the answer, and its only
        // rational move is to ask it again.
        let issue = base_issue();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            human_answers: &[(
                "Which schema should this use?".to_string(),
                "SQLite, with migration 009.".to_string(),
            )],
            remediation: None,
            gate_commands: &["cargo test".to_string()],
            n_turns: 3,
        });
        assert!(text.contains("Which schema should this use?"));
        assert!(text.contains("SQLite, with migration 009."));
        assert!(
            text.contains("decisions, not suggestions"),
            "an answer is an instruction, not a hint"
        );
    }

    #[test]
    fn an_answer_is_not_rendered_inside_the_prior_attempts_list() {
        // A human decision buried in a list of past failures gets skimmed.
        let issue = base_issue();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[PriorAttempt {
                attempt_n: 1,
                approach: "tried A".to_string(),
                verdict: AttemptOutcome::Failed,
                why_failed: Some("A did not work".to_string()),
            }],
            strategy_redirect: None,
            human_answers: &[("Q?".to_string(), "A!".to_string())],
            remediation: None,
            gate_commands: &["cargo test".to_string()],
            n_turns: 3,
        });
        let attempts_at = text.find("tried A").unwrap();
        let answer_at = text.find("A!").unwrap();
        let constraints_at = text.find("Constraints:").unwrap();
        assert!(attempts_at < answer_at && answer_at < constraints_at);
    }

    #[test]
    fn no_answers_adds_no_section_at_all() {
        let issue = base_issue();
        let text = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            human_answers: &[],
            remediation: None,
            gate_commands: &["cargo test".to_string()],
            n_turns: 3,
        });
        assert!(!text.contains("A human answered"));
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
            human_answers: &[],
            remediation: None,
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
            human_answers: &[],
            remediation: None,
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
            human_answers: &[],
            remediation: None,
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
            human_answers: &[],
            remediation: None,
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
            human_answers: &[],
            remediation: None,
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
            human_answers: &[],
            remediation: None,
            gate_commands: &["cargo test --workspace".to_string()],
            n_turns: 6,
        });
        assert!(text.chars().count() < 4_000);
    }
}
