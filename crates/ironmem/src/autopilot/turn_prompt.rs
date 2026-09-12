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
    /// Issue body, verbatim from `gh issue view --json body`.
    ///
    /// **Bounded here, not by the caller.** This field used to say the
    /// caller decided what "exceeds budget" meant; no caller ever did, and a
    /// body that pushed the condition past [`MAX_CONDITION_CHARS`] was
    /// rejected by the platform with no assistant turn at all — see
    /// [`render`]'s *Fitting the limit*. [`render`] now truncates it to fit.
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
    /// The branch this dispatch's worktree has checked out, from
    /// [`super::worktree::Worktree::branch`].
    ///
    /// Named in the goal condition rather than left implicit as "the branch
    /// you are on". A dispatch that cuts its own branch and pushes that
    /// instead satisfies every other word of the condition while leaving
    /// nothing on the branch [`super::advance`] looks at, and the failure is
    /// silent: the issue records a success and stalls with no pull request.
    pub branch: &'a str,
    /// The repo's approved gate commands, verbatim from
    /// [`super::gate_config::GateConfig::gate_commands`] — never authored
    /// separately from the approved config.
    pub gate_commands: &'a [String],
    /// Turns per dispatch (the spec's **N**). Must be at least 1; see
    /// [`render`]'s panic doc.
    pub n_turns: u32,
}

/// The platform's hard ceiling on a `/goal` condition, in characters.
///
/// **Measured, not assumed.** Dispatching ironrace/ironmem#339 with a
/// 4,463-character issue body put this on the IC's stdout and produced no
/// assistant turn whatsoever:
///
/// ```text
/// Goal condition is limited to 4000 characters (got 5521)
/// ```
///
/// The dispatch cost nothing, took no turns and returned no verdict, so it
/// classified as [`super::run`]'s `InfrastructureFailure` — a name for a
/// transient condition that, in this case, would never have cleared. See
/// issue #340.
pub const MAX_CONDITION_CHARS: usize = 4000;

/// A condition that cannot be made to fit [`MAX_CONDITION_CHARS`].
///
/// Returned rather than rendered-and-hoped, because the platform's rejection
/// is silent: an over-long condition produces no turn, no verdict and no
/// comment on the issue, and the issue keeps `agent:ready` — so the next tick
/// picks it up and fails identically, for ever. A dispatch that cannot be
/// composed must not be spent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConditionTooLong {
    /// The issue whose dispatch could not be composed.
    pub issue: String,
    /// What the condition measured after every reduction was applied.
    pub rendered: usize,
    /// [`MAX_CONDITION_CHARS`], carried so the message is self-contained.
    pub limit: usize,
}

impl std::fmt::Display for ConditionTooLong {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "goal condition for {issue} is {rendered} characters after dropping \
the issue body and every prior attempt, over the {limit}-character limit",
            issue = self.issue,
            rendered = self.rendered,
            limit = self.limit,
        )
    }
}

impl std::error::Error for ConditionTooLong {}

/// Characters, not bytes — the limit the platform enforces is stated in
/// characters, and `len()` would over-count any non-ASCII issue body and
/// truncate it harder than necessary.
fn char_len(s: &str) -> usize {
    s.chars().count()
}

/// Truncate `text` so the result *including* `marker` is at most `budget`
/// characters, cutting on a character boundary.
///
/// Returns empty when there is no room for even the marker; [`render`]'s next
/// reduction stage handles that case rather than emitting a marker with no
/// content attached to it.
fn truncate_with_marker(text: &str, budget: usize, marker: &str) -> String {
    if char_len(text) <= budget {
        return text.to_string();
    }
    let marker_len = char_len(marker);
    if budget <= marker_len {
        return String::new();
    }
    let mut out: String = text.chars().take(budget - marker_len).collect();
    out.push_str(marker);
    out
}

/// The marker left in place of the cut part of a body.
///
/// It names the issue and the command that reads it in full, which is what
/// makes truncation *recoverable*: the IC runs in a worktree with `gh`
/// available, so a shortened body costs it one tool call rather than the
/// information. A dropped attempt history has no such fallback — nothing in
/// the worktree records it — which is why the body is cut first.
fn body_truncation_marker(issue: &super::IssueRef) -> String {
    format!(
        "\n\n[... issue body truncated to fit the {MAX_CONDITION_CHARS}-character goal-condition \
limit. Read the whole issue before you start: `gh issue view {number} --repo {repo}`]",
        number = issue.number,
        repo = issue.repo,
    )
}

/// The "Prior attempts" block, keeping the `keep_newest` most recent.
///
/// Newest-first retention on purpose: when attempts must be dropped to fit,
/// the ones that matter are the most recent, and an omission the IC cannot
/// see would let it repeat an approach the record says already failed.
fn lineage_section_for(attempts: &[PriorAttempt], keep_newest: usize) -> String {
    if attempts.is_empty() {
        return "none yet".to_string();
    }
    let dropped = attempts.len().saturating_sub(keep_newest);
    let mut out = String::new();
    if dropped > 0 {
        out.push_str(&format!(
            "({dropped} earlier attempt(s) omitted so this condition fits the \
{MAX_CONDITION_CHARS}-character limit)\n"
        ));
    }
    out.push_str(
        &attempts[dropped..]
            .iter()
            .map(format_prior_attempt)
            .collect::<Vec<_>>()
            .join("\n"),
    );
    out.trim_end().to_string()
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
/// # Fitting the limit
///
/// The platform caps a condition at [`MAX_CONDITION_CHARS`] and rejects an
/// over-long one outright — no turn, no verdict, no spend, nothing written to
/// the issue. So this module reduces the condition until it fits, in a fixed
/// order: the issue body first (recoverable — the marker names the `gh`
/// command that reads it in full), then the oldest prior attempts (not
/// recoverable, which is why they go last). Everything else — the gate line,
/// the push clause, the constraints, the verdict instruction — is load-bearing
/// and is never cut.
///
/// # Errors
///
/// [`ConditionTooLong`] when the irreducible part alone exceeds the limit.
/// Refusing is the point: the alternative is a dispatch that is spent, does
/// nothing, and leaves the issue looking exactly as it did before.
///
/// # Why the push is in the condition
///
/// Because the gate alone was never the whole of "done", and for eleven rungs
/// the ordinary condition said only "the gate passes". A gate runs in the
/// worktree, so an IC that edits, commits and goes green **without pushing**
/// satisfies it completely and truthfully — and leaves the remote branch
/// empty. [`super::advance`] then finds no pull request, and because
/// [`super::run::run_issue`] refuses to dispatch an issue that already
/// records a success, there is no path back: the issue stalls on every pass,
/// forever, with no human action short of an unlabel able to recover it.
///
/// So the push is part of the condition and not merely a line of prose above
/// it, on the same reasoning as the remediation clause below: a dispatch
/// reports its verdict against *the condition*, and anything the condition
/// does not name is something an honest IC may report `met` without having
/// done. It is phrased over the commits the dispatch actually made, so an
/// issue that turns out to need no code change is still a legitimate success
/// rather than one that can only be reported by pushing an empty commit.
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
pub fn render(inputs: &TurnPromptInputs) -> Result<String, ConditionTooLong> {
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

    let lineage_section = lineage_section_for(inputs.prior_attempts, inputs.prior_attempts.len());

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
        ", and every finding in the review above is addressed by one of those commits"
    } else {
        ""
    };

    // The push half of the condition, which is NOT optional and NOT a
    // remediation-only concern — see [`render`]'s "Why the push is in the
    // condition".
    let push_clause = format!(
        ", and every commit you have made is pushed to this issue's branch `{}`",
        inputs.branch
    );

    let gate_line = inputs.gate_commands.join(" && ");

    let compose = |body: &str, lineage: &str| {
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
never authored separately): {gate}{push}{gate_extra}.\n\n\
Push the branch; do NOT open a pull request. Autopilot opens it for you, \
against the repo's default branch, once your work is on the remote — a pull \
request you open yourself can target the wrong base, and two open on one \
branch stop the issue dead.\n\n\
Report your verdict using the required output schema when, and only when, \
you have either satisfied the gate condition above or determined it cannot be \
satisfied. Do not guess; if you are unsure whether it is met, the verdict is \
not_met and you take another turn.\n\n\
or stop after {n} turns",
            issue = inputs.issue.canonical(),
            title = inputs.issue_title,
            body = body,
            lineage = lineage,
            redirect = redirect_line,
            answers = answers_section,
            remediation = remediation_section,
            gate = gate_line,
            push = push_clause,
            gate_extra = gate_extra,
            n = inputs.n_turns,
        )
    };

    // The common case: it already fits, and nothing is cut.
    let full = compose(inputs.issue_body, &lineage_section);
    if char_len(&full) <= MAX_CONDITION_CHARS {
        return Ok(full);
    }

    // Reduction 1 — the issue body, down to whatever the rest of the
    // condition leaves. Measured against a compose with an empty body rather
    // than by subtracting a guessed constant, so the budget stays correct as
    // the template changes.
    let marker = body_truncation_marker(inputs.issue);
    let budget_for_body =
        |lineage: &str| MAX_CONDITION_CHARS.saturating_sub(char_len(&compose("", lineage)));
    let clamped = compose(
        &truncate_with_marker(
            inputs.issue_body,
            budget_for_body(&lineage_section),
            &marker,
        ),
        &lineage_section,
    );
    if char_len(&clamped) <= MAX_CONDITION_CHARS {
        return Ok(clamped);
    }

    // Reduction 2 — prior attempts, oldest first. Only reached when the
    // lineage block alone will not fit, which the attempt cap makes rare and
    // free-text `approach`/`why_failed` values make possible.
    for keep in (0..inputs.prior_attempts.len()).rev() {
        let lineage = lineage_section_for(inputs.prior_attempts, keep);
        let candidate = compose(
            &truncate_with_marker(inputs.issue_body, budget_for_body(&lineage), &marker),
            &lineage,
        );
        if char_len(&candidate) <= MAX_CONDITION_CHARS {
            return Ok(candidate);
        }
    }

    // Everything reducible is gone and it still does not fit: the gate
    // commands, the title or the reviewer findings are themselves over the
    // limit. Refuse rather than spend a dispatch the platform will drop.
    let floor = compose("", &lineage_section_for(inputs.prior_attempts, 0));
    Err(ConditionTooLong {
        issue: inputs.issue.canonical(),
        rendered: char_len(&floor),
        limit: MAX_CONDITION_CHARS,
    })
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 6,
        })
        .unwrap()
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test --workspace".to_string()],
            n_turns: 6,
        })
        .unwrap();
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
            condition.contains("addressed by one of those commits"),
            "the pushed fix must be part of the CONDITION, not just the prose: {condition}"
        );
        // "those commits" is only meaningful because the push clause it
        // refers back to is unconditional. If the base clause ever became
        // remediation-aware again, this sentence would dangle.
        assert!(
            condition.find("is pushed to this issue's branch").unwrap()
                < condition.find("addressed by one of those commits").unwrap(),
            "the remediation clause refers back to the push clause and must follow it: {condition}"
        );
    }

    /// An ordinary (non-remediation) dispatch's rendered prompt.
    fn ordinary_text(branch: &str) -> String {
        let issue = base_issue();
        render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "T",
            issue_body: "B",
            prior_attempts: &[],
            strategy_redirect: None,
            human_answers: &[],
            remediation: None,
            branch,
            gate_commands: &["cargo test".to_string()],
            n_turns: 6,
        })
        .unwrap()
    }

    #[test]
    fn an_ordinary_dispatch_carries_no_remediation_wording() {
        let text = ordinary_text("autopilot/owner-repo-7");
        assert!(text.contains("never authored separately): cargo test,"));
        assert!(!text.contains("asked for CHANGES"));
        assert!(!text.contains("ALREADY MET the gate"));
        assert!(!text.contains("addressed by one of those commits"));
    }

    #[test]
    fn the_ordinary_goal_condition_requires_the_push_and_names_the_branch() {
        // The defect that blocked the first unattended run. For eleven rungs
        // the ordinary condition was the gate and nothing else, and a gate
        // runs in the worktree: an IC could edit, commit, go green and report
        // `met` truthfully without the work ever reaching the remote. Nothing
        // downstream could recover — `run_issue` will not re-dispatch an
        // issue that records a success — so the issue stalled on every
        // `advance` pass for ever.
        //
        // Asserted against the CONDITION rather than the whole prompt, for
        // the reason the remediation test is: a dispatch reports its verdict
        // against the condition, and an instruction that sits only in the
        // prose above it is one an honest IC may report `met` without having
        // followed.
        let text = ordinary_text("autopilot/owner-repo-7");
        let gate_at = text.find("The gate condition for this repo").unwrap();
        let condition = &text[gate_at..text[gate_at..].find("\n\n").unwrap() + gate_at];
        assert!(
            condition.contains("every commit you have made is pushed"),
            "the push must be part of the CONDITION, not just the prose: {condition}"
        );
        assert!(
            condition.contains("autopilot/owner-repo-7"),
            "the condition must name the branch, so a dispatch that cut its own cannot satisfy it: {condition}"
        );
    }

    #[test]
    fn the_condition_is_phrased_over_the_commits_made_not_over_a_commit_existing() {
        // An issue that turns out to need no code change is a legitimate
        // success (`run::head_commit` says so). Phrased as "a commit is
        // pushed", the condition would be unsatisfiable for it, and the only
        // way to report `met` would be to push an empty commit — which is
        // exactly what the remediation clause tells an IC never to do.
        let text = ordinary_text("autopilot/owner-repo-7");
        assert!(text.contains("every commit you have made is pushed"));
        assert!(!text.contains("you have pushed a commit"));
    }

    #[test]
    fn the_ic_is_told_not_to_open_the_pull_request_itself() {
        // `advance::open_pull_request` opens it, against the base GitHub
        // reports. An IC that opens its own can target whatever base its
        // checkout tracks, and two open pull requests on one branch are
        // `Stall::AmbiguousPr`, which fails closed and needs a human.
        let text = ordinary_text("autopilot/owner-repo-7");
        assert!(text.contains("do NOT open a pull request"));
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
        assert!(text.contains("addressed by one of those commits"));
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 6,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test --workspace".to_string()],
            n_turns: 6,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 1,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 3,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 3,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 3,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 3,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &commands,
            n_turns: 1,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 1,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test".to_string()],
            n_turns: 0,
        })
        .unwrap();
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &[],
            n_turns: 1,
        })
        .unwrap();
    }

    #[test]
    fn condition_never_exceeds_the_documented_4000_char_limit_for_a_realistic_case() {
        // ⟨r5-doc⟩: the condition may be up to 4,000 characters. This is the
        // *comfort* check — a realistic dispatch stays well under the limit
        // without any reduction being applied. The limit itself is enforced
        // by `the_condition_is_never_longer_than_the_limit_whatever_the_body_size`
        // and friends below; this test passed while nothing enforced it, which
        // is how #340 shipped.
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
            branch: "autopilot/owner-repo-7",
            gate_commands: &["cargo test --workspace".to_string()],
            n_turns: 6,
        })
        .unwrap();
        assert!(text.chars().count() < 4_000);
    }

    // ---------------------------------------------------------------
    // #340 — the condition is bounded here, and these tests fail if the
    // reduction in `render` is removed. The two tests that predate them
    // (`..._for_a_realistic_case`, `a_realistic_remediation_...`) assert a
    // hand-picked short fixture and pass with or without a clamp, which is
    // exactly why the defect reached a live dispatch.
    // ---------------------------------------------------------------

    /// Every reduction test renders through here, with this repo's real
    /// three-command gate so the overhead is the one production actually pays.
    fn condition_for(body: &str, prior: &[PriorAttempt]) -> Result<String, ConditionTooLong> {
        let issue = base_issue();
        render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "docs(autopilot): write the operator guide the subsystem has never had",
            issue_body: body,
            prior_attempts: prior,
            strategy_redirect: None,
            human_answers: &[],
            remediation: None,
            branch: "autopilot/ironrace-ironmem-339",
            gate_commands: &[
                "cargo fmt --all -- --check".to_string(),
                "cargo clippy --workspace --all-targets --all-features -- -D warnings".to_string(),
                "cargo test --workspace".to_string(),
            ],
            n_turns: 6,
        })
    }

    fn long_attempts(n: u32) -> Vec<PriorAttempt> {
        (1..=n)
            .map(|i| PriorAttempt {
                attempt_n: i,
                approach: format!("approach {i}: {}", "d".repeat(300)),
                verdict: AttemptOutcome::Failed,
                why_failed: Some(format!("why {i}: {}", "e".repeat(300))),
            })
            .collect()
    }

    #[test]
    fn the_condition_is_never_longer_than_the_limit_whatever_the_body_size() {
        // The enforcing test. Deleting the reduction in `render` fails this at
        // the 4,001-char case and every case above it.
        for size in [
            0, 1, 512, 2_900, 3_999, 4_000, 4_001, 4_463, 20_000, 200_000,
        ] {
            let text = condition_for(&"x".repeat(size), &[])
                .unwrap_or_else(|e| panic!("body of {size} chars should compose, got {e}"));
            assert!(
                text.chars().count() <= MAX_CONDITION_CHARS,
                "body of {size} chars rendered {} chars, over the {MAX_CONDITION_CHARS} limit",
                text.chars().count(),
            );
        }
    }

    #[test]
    fn the_body_that_actually_failed_issue_339_now_composes() {
        // ironrace/ironmem#339's body was 4,463 characters and produced
        // "Goal condition is limited to 4000 characters (got 5521)" — no
        // assistant turn, no verdict, three dispatches burned as
        // InfrastructureFailure.
        let text = condition_for(&"x".repeat(4_463), &[]).unwrap();
        assert!(text.chars().count() <= MAX_CONDITION_CHARS);
    }

    #[test]
    fn a_body_that_fits_is_passed_through_untouched() {
        // The clamp must not fire when it is not needed: an unnecessary
        // truncation costs the IC a tool call and the marker is a lie.
        let text = condition_for("A short issue body.", &[]).unwrap();
        assert!(text.contains("A short issue body."));
        assert!(!text.contains("truncated"));
    }

    #[test]
    fn truncation_is_visible_and_names_how_to_recover_the_body() {
        // Silent truncation is the same class of defect as the silent
        // rejection this fixes: the IC must be able to tell that it is
        // reading a shortened body, and to go read the rest.
        let text = condition_for(&"x".repeat(50_000), &[]).unwrap();
        assert!(text.contains("issue body truncated"));
        assert!(text.contains("gh issue view 283 --repo ironrace/ironmem"));
    }

    #[test]
    fn the_load_bearing_tail_survives_any_truncation() {
        // Guards against the naive fix — truncating the whole condition —
        // which would cut the gate line, the push clause and the turn bound
        // and leave an IC with no definition of done at all.
        let text = condition_for(&"x".repeat(200_000), &[]).unwrap();
        assert!(text.contains("cargo test --workspace"));
        assert!(text.contains("is pushed to this issue's branch"));
        assert!(text.contains("Report your verdict using the required output schema"));
        assert!(text.ends_with("or stop after 6 turns"));
    }

    #[test]
    fn the_clamp_holds_as_prior_attempts_accumulate() {
        // The growth path: prior attempts are appended to the condition, so an
        // issue that composed at attempt 1 can stop composing by attempt 3.
        // A dispatch loop that works and then silently stops working is worse
        // than one that never worked.
        let body = "y".repeat(2_000);
        for n in 0..=20 {
            let text = condition_for(&body, &long_attempts(n))
                .unwrap_or_else(|e| panic!("{n} prior attempts should compose, got {e}"));
            assert!(
                text.chars().count() <= MAX_CONDITION_CHARS,
                "{n} prior attempts rendered {} chars",
                text.chars().count(),
            );
        }
    }

    #[test]
    fn the_newest_attempts_are_kept_when_the_lineage_must_be_cut() {
        // Dropping the *newest* attempt would let the IC repeat the approach
        // that just failed, which is the one thing the lineage block exists
        // to prevent.
        let text = condition_for("a body", &long_attempts(30)).unwrap();
        assert!(text.chars().count() <= MAX_CONDITION_CHARS);
        assert!(text.contains("attempt 30:"));
        assert!(text.contains("omitted so this condition fits"));
        assert!(!text.contains("attempt 1:"));
    }

    #[test]
    fn a_condition_that_cannot_be_reduced_to_fit_is_refused() {
        // Nothing reducible is left and the irreducible part is still over.
        // Refusing is the point: rendering it anyway spends a dispatch the
        // platform silently drops.
        let issue = base_issue();
        let err = render(&TurnPromptInputs {
            issue: &issue,
            issue_title: "t",
            issue_body: "b",
            prior_attempts: &[],
            strategy_redirect: None,
            human_answers: &[],
            remediation: None,
            branch: "autopilot/ironrace-ironmem-339",
            gate_commands: &["cargo test ".to_string() + &"--flag ".repeat(2_000)],
            n_turns: 6,
        })
        .expect_err("an irreducibly over-long condition must be refused");
        assert_eq!(err.limit, MAX_CONDITION_CHARS);
        assert!(err.rendered > MAX_CONDITION_CHARS);
        assert!(err.to_string().contains("ironrace/ironmem#283"));
    }

    #[test]
    fn a_truncated_body_still_leaves_room_for_the_marker_itself() {
        // The budget counts the marker. A reduction that fit the body and
        // then appended the marker would overflow by exactly the marker.
        let text = condition_for(&"x".repeat(MAX_CONDITION_CHARS), &[]).unwrap();
        assert!(text.contains("issue body truncated"));
        assert!(text.chars().count() <= MAX_CONDITION_CHARS);
    }
}
