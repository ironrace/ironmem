//! Cross-dispatch stagnation control — build-ladder rung 6.
//!
//! The spec's *Cross-dispatch stagnation control* exists to stop one specific
//! livelock: an IC exhausts its retries today, the issue keeps `agent:ready`,
//! the daily budget resets at midnight, the Lead picks the same issue
//! tomorrow, and lineage prevents repeating the *same* approach but not
//! another doomed one — forever.
//!
//! The countermeasure has three parts. Rung 4 built the first (a per-issue
//! attempt counter that persists across dispatches, and a terminal lineage
//! record when it is reached — see [`super::run`]). This module is the other
//! two: **post a comment summarizing everything tried, and flip the label to
//! `agent:exhausted`**, which never self-resumes.
//!
//! # Why this is not in [`super::merge`]
//!
//! It was, and it did not belong there. Nothing here touches a
//! `MergeDecision`, a `PrSnapshot`, or a pull request at all — an issue can
//! exhaust its attempts without ever opening one. What it shares with the
//! merge path is a *notification primitive* (post a bounded, scrubbed
//! comment, then set the exclusive `agent:*` label), not a subject. Leaving
//! it there would have meant rung 7's loop depending on the merge-authority
//! module in order to close out an issue that has no PR, and every change to
//! stagnation policy re-entering the review surface of the one module that
//! performs Autopilot's only irreversible action.

use serde::Serialize;

use super::gh::{self, GhRunner};
use super::labels::{self, AgentLabel};
use super::lineage;
use super::merge::{serialize_issue, short_sha, MAX_COMMENT_CHARS};
use super::scrub::scrub_and_bound;
use super::{validate_repo, IssueRef};
use crate::db::schema::Database;
use crate::error::MemoryError;

/// How many attempts an exhaustion comment quotes, newest first.
///
/// Bounded separately from [`MAX_COMMENT_CHARS`] so the comment degrades by
/// dropping *whole oldest attempts* rather than by truncating mid-sentence in
/// the middle of the most recent one, which is the part a human reads first.
pub const MAX_ATTEMPTS_IN_COMMENT: usize = 10;

// ── stagnation ──────────────────────────────────────────────────────────

/// What [`exhaust_issue`] did.
///
/// An enum rather than a `commented: bool` beside a
/// `label_plan: Option<_>`: those two fields could spell four states for a
/// process that has three, and the impossible fourth ("commented, but no
/// label plan") had to be papered over with a catch-all arm at the one place
/// that reads them. This makes the CLI's match total.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum ExhaustOutcome {
    /// The issue already carried `agent:exhausted`. Nothing was read, posted
    /// or changed.
    AlreadyExhausted,
    /// A dry run: the summary was rendered and the plan computed, but
    /// nothing was written to GitHub.
    WouldExhaust {
        label_plan: labels::LabelPlan,
        /// How many attempts the summary drew on.
        attempts_summarized: usize,
    },
    /// The summary was posted and the label applied.
    Exhausted {
        label_plan: labels::LabelPlan,
        attempts_summarized: usize,
    },
}

/// The result of one exhaustion attempt.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ExhaustExecution {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    /// Flattened: [`ExhaustOutcome`] is already internally tagged on
    /// `"outcome"`, so nesting it under a field of the same name would emit
    /// `{"outcome":{"outcome":"exhausted",…}}` — a doubly-nested key nothing
    /// asked for. Flattening puts the tag and its payload at the top level,
    /// which is the shape the tag name was chosen for.
    #[serde(flatten)]
    pub outcome: ExhaustOutcome,
}

/// Close out an issue that hit its per-issue attempt cap: post a comment
/// summarizing everything tried, then flip the label to `agent:exhausted`.
///
/// Rung 4 already appends the terminal lineage record when the cap is
/// reached; this is the other two thirds of the spec's bullet — *"append a
/// terminal lineage record, post a comment summarizing everything tried, and
/// flip the label to `agent:exhausted`"*.
///
/// # Idempotent on purpose
///
/// An issue already carrying `agent:exhausted` is left completely alone —
/// no second comment, no label churn. Rung 4's review found the same shape as
/// a real defect (every re-run of an exhausted issue appended another
/// terminal record, each quoting all the prior ones); a poll loop calling
/// this on every pass would otherwise bury the issue in identical comments.
pub fn exhaust_issue(
    gh_runner: &mut dyn GhRunner,
    db: &Database,
    issue: &IssueRef,
    dry_run: bool,
) -> Result<ExhaustExecution, MemoryError> {
    validate_repo(&issue.repo)?;

    let current = gh::issue_labels(gh_runner, issue)?;
    let already = current
        .iter()
        .any(|l| AgentLabel::from_label_str(l) == Some(AgentLabel::Exhausted));

    if already {
        return Ok(ExhaustExecution {
            issue: issue.clone(),
            outcome: ExhaustOutcome::AlreadyExhausted,
        });
    }

    // Read only once the issue is known to need a summary — an already-
    // exhausted issue walks none of its lineage.
    let attempts = lineage::attempts_for_issue(db, issue)?;
    let body = render_exhaustion_comment(issue, &attempts);
    if dry_run {
        return Ok(ExhaustExecution {
            issue: issue.clone(),
            outcome: ExhaustOutcome::WouldExhaust {
                label_plan: labels::plan_exclusive(&current, Some(AgentLabel::Exhausted)),
                attempts_summarized: attempts.len().min(MAX_ATTEMPTS_IN_COMMENT),
            },
        });
    }

    // Comment first, then label. If the label write fails the human still has
    // the summary; if the order were reversed a failure would leave an issue
    // marked exhausted with no explanation of why, which is the worse half to
    // lose.
    gh::comment_on_issue(gh_runner, issue, &body)?;
    // `current` was read at the top of this function; re-reading it inside
    // `set_exclusive_label` would spawn a second `gh issue view` for labels
    // we are already holding.
    let plan = labels::apply_plan(
        gh_runner,
        issue,
        labels::plan_exclusive(&current, Some(AgentLabel::Exhausted)),
    )?;

    Ok(ExhaustExecution {
        issue: issue.clone(),
        outcome: ExhaustOutcome::Exhausted {
            label_plan: plan,
            attempts_summarized: attempts.len().min(MAX_ATTEMPTS_IN_COMMENT),
        },
    })
}

/// Render the exhaustion summary comment.
///
/// Newest attempts first, because that is what a human triaging the issue
/// reads: the last thing tried is the best evidence about why this is stuck.
/// Every quoted field is already scrubbed and bounded on the way *into*
/// lineage; the whole body is scrubbed again on the way out, because the
/// composition is a new string and this is the point where it leaves the
/// machine.
pub fn render_exhaustion_comment(issue: &IssueRef, attempts: &[lineage::AttemptRecord]) -> String {
    let mut body = format!(
        "**Autopilot stopped working {issue}.**\n\n\
The per-issue attempt cap was reached, so this issue is now labeled `{label}`. \
It **will not be retried automatically** — that is the point of the label. A \
human who wants another pass should re-label it `{ready}`.\n\n",
        issue = issue.canonical(),
        label = AgentLabel::Exhausted.as_str(),
        ready = AgentLabel::Ready.as_str(),
    );

    if attempts.is_empty() {
        body.push_str("No attempt records were found for this issue.\n");
    } else {
        let shown: Vec<&lineage::AttemptRecord> = attempts
            .iter()
            .rev()
            .take(MAX_ATTEMPTS_IN_COMMENT)
            .collect();
        body.push_str(&format!(
            "### What was tried ({} attempt{}, most recent first)\n\n",
            attempts.len(),
            if attempts.len() == 1 { "" } else { "s" }
        ));
        for attempt in shown {
            body.push_str(&format!(
                "- **Attempt {n}** — {verdict:?}\n  - approach: {approach}\n",
                n = attempt.attempt_n,
                verdict = attempt.verdict,
                approach = one_line(&attempt.approach),
            ));
            if let Some(why) = &attempt.why_failed {
                body.push_str(&format!("  - why it failed: {}\n", one_line(why)));
            }
            if let Some(sha) = &attempt.commit_sha {
                body.push_str(&format!("  - commit: `{}`\n", short_sha(sha)));
            }
        }
        if attempts.len() > MAX_ATTEMPTS_IN_COMMENT {
            body.push_str(&format!(
                "\n<sub>{} older attempt(s) omitted; the full lineage is in Autopilot's \
knowledge base.</sub>\n",
                attempts.len() - MAX_ATTEMPTS_IN_COMMENT
            ));
        }
    }

    body.push_str("\n<sub>Autopilot rung 6.</sub>");
    scrub_and_bound(&body, MAX_COMMENT_CHARS).text
}

/// Flatten a multi-line field into one Markdown list line.
///
/// Newlines in an attempt's `approach` or `why_failed` would otherwise break
/// out of the bullet they belong to and reflow the rest of the comment; a
/// leading `#` or `-` on a wrapped line would even render as a new heading or
/// list item. Bounded here as well as at the whole-comment level so one
/// enormous field cannot crowd out every other attempt.
///
/// The collapse itself is [`crate::sanitize::collapse_whitespace_and_control`],
/// the crate's shared "neutralize text before it reaches a human-visible
/// line" helper, whose doc describes exactly this layering (each caller adds
/// its own truncation on top). Hand-rolling it here mapped `\n` and `\r` only
/// and passed every other control character — `ESC`, `NUL`, `\x0b` — straight
/// through into a GitHub comment body.
///
/// `strip_invisible: true` because this string is rendered to a human on a
/// public issue and is composed from model-authored `approach` / `why_failed`
/// text that quotes the diff. `char::is_control` covers only category `Cc`,
/// so a bidi override or zero-width joiner (`Cf`) would otherwise survive and
/// visually reorder or hide the rest of the summary — Trojan-Source-style
/// spoofing in the one artifact a human reads to decide what to do next.
/// [`scrub_and_bound`] does not help here: it redacts secrets, not
/// [`crate::sanitize::is_forgeable_invisible`] characters.
fn one_line(text: &str) -> String {
    const MAX_FIELD_IN_COMMENT: usize = 500;
    // `trim`, not just the collapse: `collapse_whitespace_and_control`'s
    // internal `value.trim()` only strips `White_Space`, so a field starting
    // with a control or `Cf` character collapses to a *leading space* that
    // would be rendered inside the bullet. The hand-rolled version this
    // replaced could not produce one.
    let collapsed = crate::sanitize::collapse_whitespace_and_control(text, true)
        .trim()
        .to_string();
    if collapsed.chars().count() > MAX_FIELD_IN_COMMENT {
        let head: String = collapsed.chars().take(MAX_FIELD_IN_COMMENT).collect();
        format!("{head}…")
    } else {
        collapsed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::GhOutput;
    use crate::autopilot::lineage::{AttemptOutcome, AttemptRecord};

    fn issue() -> IssueRef {
        IssueRef::new("owner/repo", 42)
    }

    fn db() -> Database {
        Database::open_in_memory().expect("in-memory db")
    }

    fn attempt(n: u32, verdict: AttemptOutcome, why: Option<&str>) -> AttemptRecord {
        AttemptRecord {
            issue: issue(),
            attempt_n: n,
            approach: format!("approach number {n}"),
            verdict,
            why_failed: why.map(|w| w.to_string()),
            commit_sha: None,
        }
    }

    #[test]
    fn exhausting_an_issue_comments_then_labels() {
        let database = db();
        for n in 1..=3 {
            lineage::record_attempt(&database, &attempt(n, AttemptOutcome::Failed, Some("red")))
                .unwrap();
        }
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:ready"}]}"#)),
            Ok(GhOutput::ok("")),
            // `agent:exhausted` is provisioned before it is applied
            Ok(GhOutput::failed("", "label already exists")),
            Ok(GhOutput::ok("")),
        ]);

        let exec = exhaust_issue(&mut gh_runner, &database, &issue(), false).unwrap();

        assert_eq!(
            gh_runner.seen.len(),
            4,
            "one label read, one comment, one create, one edit — the labels \
are read once, not again inside the label write: {:?}",
            gh_runner.seen
        );

        let ExhaustOutcome::Exhausted {
            label_plan,
            attempts_summarized,
        } = exec.outcome
        else {
            panic!("expected Exhausted, got {:?}", exec.outcome);
        };
        assert_eq!(attempts_summarized, 3);
        assert_eq!(label_plan.add, vec!["agent:exhausted".to_string()]);
        assert_eq!(label_plan.remove, vec!["agent:ready".to_string()]);
        // The comment is posted before the label: losing the label leaves the
        // human an explanation; losing the comment leaves a bare stop sign.
        assert!(gh_runner.seen[1].contains(&"comment".to_string()));
        assert!(
            gh_runner.seen[3].contains(&"edit".to_string()),
            "{:?}",
            gh_runner.seen[3]
        );
    }

    #[test]
    fn an_already_exhausted_issue_is_left_completely_alone() {
        // A poll loop calling this every pass would otherwise bury the issue
        // in identical comments — rung 4 found the same shape as a real bug.
        let database = db();
        let mut gh_runner = ScriptedGh::new(vec![Ok(GhOutput::ok(
            r#"{"labels":[{"name":"agent:exhausted"}]}"#,
        ))]);

        let exec = exhaust_issue(&mut gh_runner, &database, &issue(), false).unwrap();

        assert_eq!(exec.outcome, ExhaustOutcome::AlreadyExhausted);
        assert_eq!(gh_runner.seen.len(), 1, "only the label read happened");
    }

    #[test]
    fn an_exhaustion_dry_run_writes_nothing() {
        let database = db();
        let mut gh_runner = ScriptedGh::new(vec![Ok(GhOutput::ok(
            r#"{"labels":[{"name":"agent:ready"}]}"#,
        ))]);

        let exec = exhaust_issue(&mut gh_runner, &database, &issue(), true).unwrap();

        assert_eq!(gh_runner.seen.len(), 1, "nothing was written");
        let ExhaustOutcome::WouldExhaust { label_plan, .. } = exec.outcome else {
            panic!("expected WouldExhaust, got {:?}", exec.outcome);
        };
        assert_eq!(label_plan.add, vec!["agent:exhausted".to_string()]);
    }

    #[test]
    fn the_exhaustion_comment_states_that_it_never_self_resumes() {
        let attempts = vec![attempt(1, AttemptOutcome::Failed, Some("tests red"))];
        let body = render_exhaustion_comment(&issue(), &attempts);
        assert!(body.contains("agent:exhausted"));
        assert!(body.contains("will not be retried automatically"));
        assert!(body.contains("agent:ready"), "the way back must be stated");
        assert!(body.contains("tests red"));
    }

    #[test]
    fn the_exhaustion_comment_lists_newest_attempts_first() {
        let attempts: Vec<AttemptRecord> = (1..=3)
            .map(|n| attempt(n, AttemptOutcome::Failed, None))
            .collect();
        let body = render_exhaustion_comment(&issue(), &attempts);
        let third = body
            .find("approach number 3")
            .expect("newest attempt shown");
        let first = body
            .find("approach number 1")
            .expect("oldest attempt shown");
        assert!(third < first, "most recent first: {body}");
    }

    #[test]
    fn the_exhaustion_comment_bounds_how_many_attempts_it_quotes() {
        let attempts: Vec<AttemptRecord> = (1..=25)
            .map(|n| attempt(n, AttemptOutcome::Failed, None))
            .collect();
        let body = render_exhaustion_comment(&issue(), &attempts);
        assert!(body.contains("25 attempts"), "the real count is stated");
        assert!(body.contains("older attempt(s) omitted"));
        assert!(
            !body.contains("approach number 1\n"),
            "the oldest attempts are dropped whole, not truncated"
        );
        assert!(body.chars().count() <= MAX_COMMENT_CHARS);
    }

    #[test]
    fn a_multiline_attempt_field_cannot_break_the_comment_layout() {
        // A wrapped line beginning `#` or `-` would render as a new heading
        // or list item and reflow everything after it.
        let attempts = vec![attempt(
            1,
            AttemptOutcome::Failed,
            Some("line one\n# not a heading\n- not a bullet"),
        )];
        let body = render_exhaustion_comment(&issue(), &attempts);
        let why_line = body
            .lines()
            .find(|l| l.contains("why it failed"))
            .expect("the field is rendered");
        assert!(why_line.contains("# not a heading"));
        assert!(
            !body.contains("\n# not a heading"),
            "the newline must not survive: {body}"
        );
    }

    #[test]
    fn an_invisible_control_character_cannot_reorder_the_comment() {
        // Category `Cf` — a bidi override — is not `char::is_control`, so it
        // survives the collapse unless `strip_invisible` is set, and would
        // visually reverse everything after it on the line a human reads.
        let attempts = vec![attempt(
            1,
            AttemptOutcome::Failed,
            Some("\u{202E}gnitset\u{200B} deliaf"),
        )];
        let body = render_exhaustion_comment(&issue(), &attempts);
        assert!(
            !body.contains('\u{202E}') && !body.contains('\u{200B}'),
            "no forgeable invisible may reach the comment: {body:?}"
        );
    }

    #[test]
    fn the_json_shape_puts_the_outcome_tag_at_the_top_level() {
        // A doubly-nested `"outcome":{"outcome":…}` is what an un-flattened
        // internally-tagged enum under a same-named field produces.
        let exec = ExhaustExecution {
            issue: issue(),
            outcome: ExhaustOutcome::AlreadyExhausted,
        };
        let json: serde_json::Value = serde_json::to_value(&exec).unwrap();
        assert_eq!(json["outcome"], serde_json::json!("already_exhausted"));
        assert_eq!(json["issue"], serde_json::json!("owner/repo#42"));
    }

    #[test]
    fn an_issue_with_no_attempts_still_produces_a_usable_comment() {
        let body = render_exhaustion_comment(&issue(), &[]);
        assert!(body.contains("No attempt records"));
        assert!(body.contains("agent:exhausted"));
    }
}
