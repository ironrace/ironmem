//! The human-question round trip — build-ladder rung 8.
//!
//! Two rows of the spec's error table, and they are two halves of one
//! mechanism:
//!
//! > IC hits a human-only decision → Lead ... posts the question on the
//! > issue, flips to `agent:blocked`.
//!
//! > ⟨r2⟩ Human answers a blocked issue → Lead polls `agent:blocked` issues
//! > for human comments newer than its own question, appends the answer to
//! > lineage, flips back to `agent:ready`, re-dispatches. **Closes the
//! > one-way door rev 1 left open.**
//!
//! # Why both halves are here rather than only the poll
//!
//! The poll is the Lead responsibility rung 8 owes. But it recognizes an
//! answer *by position relative to Autopilot's own question*, so a poll built
//! without the asking half could never fire on anything — this ladder's
//! lesson 17, which has now bitten it three times (rung 5's dollar ceiling on
//! an unpriced reviewer, rung 6's `BaseBranchMismatch` comparing a field
//! nothing populated, rung 7's `Escalate` that stopped nothing). Both halves
//! ship together, and [`ask_human`] is reachable from the CLI so the round
//! trip can be exercised end to end.
//!
//! # Recognizing the answer: position, not authorship
//!
//! The obvious rule — "a comment by someone other than us" — does not work
//! here and would fail in the one direction that matters. Autopilot comments
//! through `gh`, which authenticates as *a person's* account; on this
//! machine that is the same human who would be answering. An author check
//! would therefore read Jeff's answer as Autopilot's own comment and block
//! the issue forever.
//!
//! So the rule is positional and author-independent: Autopilot stamps every
//! question with [`QUESTION_MARKER`], an HTML comment that renders invisibly
//! on GitHub, and an **answer** is any comment that comes after the newest
//! marked question and does not itself carry the marker. That is correct
//! whichever account either party posts from.
//!
//! The marker is load-bearing enough to be worth stating what its absence
//! does: an `agent:blocked` issue with no marked question is **held**, never
//! unblocked. A hand-blocked issue is a human deliberately stopping work, and
//! guessing that the first comment on it was an answer would restart work a
//! human meant to stop.
//!
//! # A ninth drawer kind
//!
//! [`BlockedRecord`] is kind 2's shape (`logical_key` per issue). It holds
//! the bounded question/answer history so an answer is not merely *observed*
//! but **delivered**: [`active_answers`] feeds
//! [`super::turn_prompt::TurnPromptInputs::human_answers`], which is what
//! makes "appends the answer to lineage" a real transfer of information to
//! the next dispatch rather than a label flip and a log line.
//!
//! The answer is deliberately **not** recorded as a
//! [`super::lineage::AttemptRecord`]. An attempt is a dispatch that ran; an
//! answer is context. Filing it as an attempt would consume one of the
//! issue's bounded attempts and enter rung 7's thrash-detection window as a
//! failure with no `why_failed` — paying twice for the act of asking.

use serde::{Deserialize, Serialize};

use super::gh::{self, GhRunner, IssueComment};
use super::labels::{self, AgentLabel};
use super::merge::MAX_COMMENT_CHARS;
use super::scrub::scrub_and_bound;
use super::{read_current, validate_repo, write_current, IssueRef};
use crate::db::schema::Database;
use crate::error::MemoryError;

/// The invisible stamp that identifies a comment as Autopilot's own question.
///
/// An HTML comment: GitHub renders it as nothing at all, so a human reading
/// the issue sees only the question. Kept on one line and free of
/// user-supplied text so it cannot be broken up by a question body that
/// happens to contain a newline or an angle bracket.
pub const QUESTION_MARKER: &str = "<!-- autopilot:question -->";

/// How many question/answer pairs an issue's record keeps.
///
/// Bounded because every pair is rendered into every subsequent turn prompt,
/// and an unbounded history would grow the prompt — and therefore each
/// dispatch's input cost — without limit. Oldest pairs are dropped first: a
/// later answer supersedes an earlier one on the same issue far more often
/// than the reverse.
pub const MAX_QA_PAIRS: usize = 5;

/// The longest question or answer text kept per pair.
///
/// Separate from [`MAX_COMMENT_CHARS`] because these are re-rendered into a
/// turn prompt, where a 20k-character answer would crowd out the lineage and
/// gate sections the prompt exists to carry.
pub const MAX_QA_CHARS: usize = 2_000;

/// One question Autopilot asked, and the human's answer if it has arrived.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QaPair {
    pub question: String,
    pub asked_at: String,
    /// `None` while the issue is still waiting on a human.
    #[serde(default)]
    pub answer: Option<String>,
    #[serde(default)]
    pub answered_at: Option<String>,
}

impl QaPair {
    fn is_answered(&self) -> bool {
        self.answer.is_some()
    }
}

/// An issue's question/answer history. Ninth drawer kind, kind-2 shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockedRecord {
    pub issue: IssueRef,
    /// Oldest first, at most [`MAX_QA_PAIRS`] entries.
    pub pairs: Vec<QaPair>,
}

#[derive(Serialize, Deserialize)]
struct BlockedBody {
    repo: String,
    issue: u64,
    #[serde(default)]
    pairs: Vec<QaPair>,
}

fn blocked_key(issue: &IssueRef) -> String {
    format!("blocked:{}", issue.slug())
}

/// Write (overwrite) an issue's blocked record.
pub fn upsert_blocked(db: &Database, record: &BlockedRecord) -> Result<String, MemoryError> {
    validate_repo(&record.issue.repo)?;
    let body = BlockedBody {
        repo: record.issue.repo.clone(),
        issue: record.issue.number,
        pairs: record.pairs.clone(),
    };
    write_current(
        db,
        &blocked_key(&record.issue),
        &serde_json::to_string(&body)?,
    )
}

/// Read an issue's blocked record, if it has ever been asked a question.
pub fn get_blocked(db: &Database, issue: &IssueRef) -> Result<Option<BlockedRecord>, MemoryError> {
    let Some(drawer) = read_current(db, &blocked_key(issue))? else {
        return Ok(None);
    };
    let body: BlockedBody = serde_json::from_str(&drawer.content)?;
    Ok(Some(BlockedRecord {
        issue: IssueRef::new(body.repo, body.issue),
        pairs: body.pairs,
    }))
}

/// Every answered question on an issue, oldest first.
///
/// [`super::run::run_issue`] calls this to fill
/// [`super::turn_prompt::TurnPromptInputs::human_answers`]. Unanswered pairs
/// are excluded: telling an IC what it asked, without the answer, is noise
/// it can only act on by asking again.
pub fn active_answers(db: &Database, issue: &IssueRef) -> Result<Vec<QaPair>, MemoryError> {
    Ok(get_blocked(db, issue)?
        .map(|record| {
            record
                .pairs
                .into_iter()
                .filter(QaPair::is_answered)
                .collect()
        })
        .unwrap_or_default())
}

/// Render the comment body for a question, marker included.
///
/// The marker leads rather than trails: a body long enough to be truncated by
/// [`MAX_COMMENT_CHARS`] must not lose the one thing [`poll_answer`] reads it
/// by. Truncating a question is recoverable — the human asks what was meant;
/// truncating the marker would silently strand the issue.
pub fn render_question_comment(issue: &IssueRef, question: &str) -> String {
    let body = format!(
        "{QUESTION_MARKER}\n**Autopilot needs a human decision on {issue}.**\n\n\
{question}\n\n\
Reply on this issue and Autopilot will pick your answer up on its next pass, \
append it to the issue's lineage, and re-label the issue `{ready}` to resume \
work. The issue stays `{blocked}` until then.\n\n\
<sub>Autopilot rung 8. Any reply after this comment counts as the answer.</sub>",
        issue = issue.canonical(),
        ready = AgentLabel::Ready.as_str(),
        blocked = AgentLabel::Blocked.as_str(),
    );
    scrub_and_bound(&body, MAX_COMMENT_CHARS).text
}

/// What [`ask_human`] did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum AskOutcome {
    /// The question was posted and the issue is now `agent:blocked`.
    Asked { question: String },
    /// The issue is already waiting on a human for an unanswered question.
    /// Nothing was posted and no label was touched.
    AlreadyWaiting { question: String },
    /// Nothing was written; `--dry-run`.
    DryRun { question: String },
}

/// Post a question on an issue and flip it to `agent:blocked`.
///
/// # Ordering
///
/// The comment is posted **before** the label is set, and the record is
/// written between them. Rung 6's lesson — order the write against the half
/// you cannot afford to lose — points this way unambiguously here. If the
/// label write fails after the comment posts, the issue keeps `agent:ready`
/// and is re-dispatched: the IC sees its own question on the issue and may
/// ask again, which is noisy but self-correcting. The inverse order fails
/// into the shape rung 6's hold-comment idempotence exists to prevent: an
/// issue labeled `agent:blocked` with no question on it, waiting on a human
/// who has not been told what for, and which nothing will ever resume.
///
/// # Idempotence
///
/// A poll loop calls this. An issue that already has an unanswered question
/// is left completely alone — no second comment, no label churn — the same
/// rule rung 6's `exhaust_issue` and hold comments follow, arrived at from
/// the same direction: `agent:blocked` does not self-resolve, so a naive
/// re-ask buries the question it wants answered.
pub fn ask_human(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    issue: &IssueRef,
    question: &str,
    dry_run: bool,
) -> Result<AskOutcome, MemoryError> {
    validate_repo(&issue.repo)?;
    let question = scrub_and_bound(question.trim(), MAX_QA_CHARS).text;
    if question.is_empty() {
        return Err(MemoryError::Validation(
            "question must not be empty — a blocked issue with no question asked \
             cannot be answered"
                .into(),
        ));
    }

    let mut record = get_blocked(db, issue)?.unwrap_or_else(|| BlockedRecord {
        issue: issue.clone(),
        pairs: Vec::new(),
    });
    if let Some(pending) = record.pairs.last() {
        if !pending.is_answered() {
            return Ok(AskOutcome::AlreadyWaiting {
                question: pending.question.clone(),
            });
        }
    }

    if dry_run {
        return Ok(AskOutcome::DryRun { question });
    }

    gh::comment_on_issue(gh_runner, issue, &render_question_comment(issue, &question))?;

    record.pairs.push(QaPair {
        question: question.clone(),
        asked_at: chrono::Utc::now().to_rfc3339(),
        answer: None,
        answered_at: None,
    });
    trim_pairs(&mut record.pairs);
    upsert_blocked(db, &record)?;

    // `apply_plan`, not `set_exclusive_label`: the labels were just read, and
    // paying for a second `gh issue view` would also open a window in which
    // the plan is computed from labels other than the ones it is applied to.
    let current = gh::issue_labels(gh_runner, issue)?;
    labels::apply_plan(
        gh_runner,
        issue,
        labels::plan_exclusive(&current, Some(AgentLabel::Blocked)),
    )?;

    Ok(AskOutcome::Asked { question })
}

/// What one poll of a blocked issue concluded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "poll", rename_all = "snake_case")]
pub enum BlockedPoll {
    /// The issue is not blocked. Nothing to poll.
    NotBlocked,
    /// Blocked, and the human has not answered yet.
    StillWaiting { question: String },
    /// A human answered. The answer is recorded and the issue is back to
    /// `agent:ready`.
    Answered { question: String, answer: String },
    /// Blocked with no question Autopilot recognizes. **Held, not
    /// unblocked** — see the module doc.
    NoQuestionFound { reason: String },
    /// An answer is present but nothing was written; `--dry-run`.
    DryRun { question: String, answer: String },
}

/// Poll one `agent:blocked` issue for a human answer.
///
/// # Ordering
///
/// The record is written **before** the label flips back to `agent:ready`.
/// If the label write then fails, the answer is durably stored and the next
/// poll retries the flip; the reverse order would put the issue back in the
/// queue carrying an answer no turn prompt has, so the IC would re-ask the
/// question a human has already answered.
pub fn poll_answer(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    issue: &IssueRef,
    dry_run: bool,
) -> Result<BlockedPoll, MemoryError> {
    validate_repo(&issue.repo)?;

    let current = gh::issue_labels(gh_runner, issue)?;
    if !current
        .iter()
        .any(|l| AgentLabel::from_label_str(l) == Some(AgentLabel::Blocked))
    {
        return Ok(BlockedPoll::NotBlocked);
    }

    let comments = gh::issue_comments(gh_runner, issue)?;
    let Some(question_idx) = newest_question_index(&comments) else {
        return Ok(BlockedPoll::NoQuestionFound {
            reason: format!(
                "{} is labeled `{}` but carries no Autopilot question comment; \
                 a human blocked it deliberately, so Autopilot holds rather than \
                 guessing that a comment on it was an answer",
                issue.canonical(),
                AgentLabel::Blocked.as_str()
            ),
        });
    };

    let Some(answer_comment) = comments
        .iter()
        .skip(question_idx + 1)
        .find(|c| !is_autopilot_question(c) && !c.body.trim().is_empty())
    else {
        return Ok(BlockedPoll::StillWaiting {
            question: question_text(&comments[question_idx]),
        });
    };

    let question = question_text(&comments[question_idx]);
    let answer = scrub_and_bound(answer_comment.body.trim(), MAX_QA_CHARS).text;

    if dry_run {
        return Ok(BlockedPoll::DryRun { question, answer });
    }

    let mut record = get_blocked(db, issue)?.unwrap_or_else(|| BlockedRecord {
        issue: issue.clone(),
        pairs: Vec::new(),
    });
    // The record's own pending pair is the authority on what was asked when
    // there is one: it holds the question as Autopilot composed it, before
    // GitHub rendering or comment truncation. The comment thread is the
    // fallback for an issue asked by a different machine or before this
    // record existed.
    match record.pairs.last_mut() {
        Some(pending) if !pending.is_answered() => {
            pending.answer = Some(answer.clone());
            pending.answered_at = Some(answer_comment.created_at.clone());
        }
        _ => record.pairs.push(QaPair {
            question: question.clone(),
            asked_at: comments[question_idx].created_at.clone(),
            answer: Some(answer.clone()),
            answered_at: Some(answer_comment.created_at.clone()),
        }),
    }
    trim_pairs(&mut record.pairs);
    upsert_blocked(db, &record)?;

    labels::apply_plan(
        gh_runner,
        issue,
        labels::plan_exclusive(&current, Some(AgentLabel::Ready)),
    )?;

    Ok(BlockedPoll::Answered { question, answer })
}

/// Whether a comment is one of Autopilot's own questions.
fn is_autopilot_question(comment: &IssueComment) -> bool {
    comment.body.contains(QUESTION_MARKER)
}

/// Index of the newest Autopilot question in a thread `gh` returned oldest
/// first.
fn newest_question_index(comments: &[IssueComment]) -> Option<usize> {
    comments.iter().rposition(is_autopilot_question)
}

/// The question text as it appears in a posted comment, marker stripped.
fn question_text(comment: &IssueComment) -> String {
    let stripped = comment.body.replace(QUESTION_MARKER, "");
    scrub_and_bound(stripped.trim(), MAX_QA_CHARS).text
}

/// Keep the newest [`MAX_QA_PAIRS`] pairs.
fn trim_pairs(pairs: &mut Vec<QaPair>) {
    if pairs.len() > MAX_QA_PAIRS {
        let drop = pairs.len() - MAX_QA_PAIRS;
        pairs.drain(..drop);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::GhOutput;

    fn issue() -> IssueRef {
        IssueRef::new("ironrace/ironmem", 283)
    }

    fn labels_json(names: &[&str]) -> String {
        let items: Vec<String> = names
            .iter()
            .map(|n| format!(r#"{{"name":"{n}"}}"#))
            .collect();
        format!(r#"{{"labels":[{}]}}"#, items.join(","))
    }

    fn comments_json(comments: &[(&str, &str, &str)]) -> String {
        let items: Vec<String> = comments
            .iter()
            .map(|(author, body, at)| {
                format!(
                    r#"{{"author":{{"login":"{author}"}},"body":{},"createdAt":"{at}"}}"#,
                    serde_json::to_string(body).unwrap()
                )
            })
            .collect();
        format!(r#"{{"comments":[{}]}}"#, items.join(","))
    }

    fn question_comment(text: &str) -> String {
        format!("{QUESTION_MARKER}\n{text}")
    }

    #[test]
    fn a_question_comment_carries_the_marker_before_anything_truncatable() {
        let body = render_question_comment(&issue(), "Which database should this use?");
        assert!(
            body.starts_with(QUESTION_MARKER),
            "marker must lead: {body}"
        );
        assert!(body.contains("Which database should this use?"));
        assert!(body.contains(AgentLabel::Ready.as_str()));
    }

    #[test]
    fn asking_posts_the_comment_then_labels_and_records_the_question() {
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("")),                             // issue comment
            Ok(GhOutput::ok(&labels_json(&["agent:ready"]))), // issue view labels
            Ok(GhOutput::ok("")),                             // gh label create
            Ok(GhOutput::ok("")),                             // issue edit
        ]);

        let outcome = ask_human(&db, &mut gh_runner, &issue(), "Which schema?", false).unwrap();
        assert_eq!(
            outcome,
            AskOutcome::Asked {
                question: "Which schema?".to_string()
            }
        );

        // The comment is the FIRST call: a label written before a question
        // that then fails to post buries the human.
        assert_eq!(gh_runner.seen[0][0], "issue");
        assert_eq!(gh_runner.seen[0][1], "comment");
        let edit = gh_runner.seen.last().unwrap();
        assert!(edit.contains(&"--add-label".to_string()));
        assert!(edit.contains(&AgentLabel::Blocked.as_str().to_string()));

        let record = get_blocked(&db, &issue()).unwrap().unwrap();
        assert_eq!(record.pairs.len(), 1);
        assert_eq!(record.pairs[0].question, "Which schema?");
        assert!(record.pairs[0].answer.is_none());
    }

    #[test]
    fn asking_twice_about_an_unanswered_question_posts_nothing() {
        let db = Database::open_in_memory().unwrap();
        upsert_blocked(
            &db,
            &BlockedRecord {
                issue: issue(),
                pairs: vec![QaPair {
                    question: "Which schema?".to_string(),
                    asked_at: "2026-09-02T00:00:00Z".to_string(),
                    answer: None,
                    answered_at: None,
                }],
            },
        )
        .unwrap();

        let mut gh_runner = ScriptedGh::new(vec![]);
        let outcome = ask_human(&db, &mut gh_runner, &issue(), "Which schema?", false).unwrap();
        assert_eq!(
            outcome,
            AskOutcome::AlreadyWaiting {
                question: "Which schema?".to_string()
            }
        );
        assert!(
            gh_runner.seen.is_empty(),
            "an issue already waiting on a human must not be commented on again"
        );
    }

    #[test]
    fn an_empty_question_is_refused() {
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![]);
        assert!(ask_human(&db, &mut gh_runner, &issue(), "   ", false).is_err());
        assert!(gh_runner.seen.is_empty());
    }

    #[test]
    fn a_reply_from_the_same_account_that_asked_still_counts_as_the_answer() {
        // The load-bearing case: `gh` authenticates as a person, so
        // Autopilot's question and the human's answer routinely share an
        // author. An author-based rule would block this issue forever.
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[
                (
                    "jeff",
                    &question_comment("Which schema?"),
                    "2026-09-02T00:00:00Z",
                ),
                ("jeff", "Use SQLite.", "2026-09-02T01:00:00Z"),
            ]))),
            Ok(GhOutput::ok("")), // gh label create
            Ok(GhOutput::ok("")), // issue edit
        ]);

        let poll = poll_answer(&db, &mut gh_runner, &issue(), false).unwrap();
        match poll {
            BlockedPoll::Answered { answer, .. } => assert_eq!(answer, "Use SQLite."),
            other => panic!("expected an answer, got {other:?}"),
        }
    }

    #[test]
    fn an_answered_issue_is_relabeled_ready_and_the_answer_reaches_the_next_prompt() {
        let db = Database::open_in_memory().unwrap();
        upsert_blocked(
            &db,
            &BlockedRecord {
                issue: issue(),
                pairs: vec![QaPair {
                    question: "Which schema?".to_string(),
                    asked_at: "2026-09-02T00:00:00Z".to_string(),
                    answer: None,
                    answered_at: None,
                }],
            },
        )
        .unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[
                (
                    "bot",
                    &question_comment("Which schema?"),
                    "2026-09-02T00:00:00Z",
                ),
                ("jeff", "Use SQLite.", "2026-09-02T01:00:00Z"),
            ]))),
            Ok(GhOutput::ok("")), // gh label create
            Ok(GhOutput::ok("")), // issue edit
        ]);

        poll_answer(&db, &mut gh_runner, &issue(), false).unwrap();

        let edit = gh_runner.seen.last().unwrap();
        assert!(edit.contains(&"--add-label".to_string()));
        assert!(edit.contains(&AgentLabel::Ready.as_str().to_string()));
        assert!(edit.contains(&"--remove-label".to_string()));

        // The delivery half: without this the re-dispatched IC would ask the
        // same question again.
        let answers = active_answers(&db, &issue()).unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].answer.as_deref(), Some("Use SQLite."));
        assert_eq!(answers[0].question, "Which schema?");
    }

    #[test]
    fn a_question_with_no_reply_yet_stays_blocked() {
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[(
                "bot",
                &question_comment("Which schema?"),
                "2026-09-02T00:00:00Z",
            )]))),
        ]);
        assert!(matches!(
            poll_answer(&db, &mut gh_runner, &issue(), false).unwrap(),
            BlockedPoll::StillWaiting { .. }
        ));
        assert_eq!(
            gh_runner.seen.len(),
            2,
            "an unanswered issue must not be relabeled"
        );
    }

    #[test]
    fn a_comment_before_the_question_is_not_an_answer() {
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[
                ("jeff", "Unrelated chatter.", "2026-09-01T00:00:00Z"),
                (
                    "bot",
                    &question_comment("Which schema?"),
                    "2026-09-02T00:00:00Z",
                ),
            ]))),
        ]);
        assert!(matches!(
            poll_answer(&db, &mut gh_runner, &issue(), false).unwrap(),
            BlockedPoll::StillWaiting { .. }
        ));
    }

    #[test]
    fn a_second_question_supersedes_an_answer_to_the_first() {
        // Only comments after the NEWEST question count. Otherwise a stale
        // answer to question 1 would immediately "answer" question 2.
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[
                ("bot", &question_comment("First?"), "2026-09-01T00:00:00Z"),
                ("jeff", "Answer to the first.", "2026-09-01T01:00:00Z"),
                ("bot", &question_comment("Second?"), "2026-09-02T00:00:00Z"),
            ]))),
        ]);
        assert!(matches!(
            poll_answer(&db, &mut gh_runner, &issue(), false).unwrap(),
            BlockedPoll::StillWaiting { .. }
        ));
    }

    #[test]
    fn a_hand_blocked_issue_with_no_autopilot_question_is_held_not_unblocked() {
        // A human blocked this deliberately. Guessing that a comment on it
        // was an answer would restart work a human meant to stop.
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[(
                "jeff",
                "Holding this until the API lands.",
                "2026-09-02T00:00:00Z",
            )]))),
        ]);
        assert!(matches!(
            poll_answer(&db, &mut gh_runner, &issue(), false).unwrap(),
            BlockedPoll::NoQuestionFound { .. }
        ));
        assert_eq!(
            gh_runner.seen.len(),
            2,
            "a held issue must not be relabeled"
        );
    }

    #[test]
    fn an_issue_that_is_not_blocked_is_not_polled_further() {
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![Ok(GhOutput::ok(&labels_json(&["agent:ready"])))]);
        assert_eq!(
            poll_answer(&db, &mut gh_runner, &issue(), false).unwrap(),
            BlockedPoll::NotBlocked
        );
        assert_eq!(gh_runner.seen.len(), 1);
    }

    #[test]
    fn a_blank_reply_is_not_an_answer() {
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[
                (
                    "bot",
                    &question_comment("Which schema?"),
                    "2026-09-02T00:00:00Z",
                ),
                ("jeff", "   ", "2026-09-02T01:00:00Z"),
            ]))),
        ]);
        assert!(matches!(
            poll_answer(&db, &mut gh_runner, &issue(), false).unwrap(),
            BlockedPoll::StillWaiting { .. }
        ));
    }

    #[test]
    fn a_dry_run_writes_nothing() {
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[
                (
                    "bot",
                    &question_comment("Which schema?"),
                    "2026-09-02T00:00:00Z",
                ),
                ("jeff", "Use SQLite.", "2026-09-02T01:00:00Z"),
            ]))),
        ]);
        assert!(matches!(
            poll_answer(&db, &mut gh_runner, &issue(), true).unwrap(),
            BlockedPoll::DryRun { .. }
        ));
        assert_eq!(gh_runner.seen.len(), 2, "a rehearsal must not relabel");
        assert!(get_blocked(&db, &issue()).unwrap().is_none());
    }

    #[test]
    fn a_secret_in_an_answer_is_scrubbed_before_it_is_stored() {
        let db = Database::open_in_memory().unwrap();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok(&labels_json(&["agent:blocked"]))),
            Ok(GhOutput::ok(&comments_json(&[
                (
                    "bot",
                    &question_comment("Which token?"),
                    "2026-09-02T00:00:00Z",
                ),
                (
                    "jeff",
                    "Use ghp_0123456789abcdefghijklmnopqrstuvwxyzAB for it.",
                    "2026-09-02T01:00:00Z",
                ),
            ]))),
            Ok(GhOutput::ok("")), // gh label create
            Ok(GhOutput::ok("")), // issue edit
        ]);
        poll_answer(&db, &mut gh_runner, &issue(), false).unwrap();
        let answers = active_answers(&db, &issue()).unwrap();
        assert!(
            !answers[0]
                .answer
                .as_deref()
                .unwrap()
                .contains("ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"),
            "an answer reaches a turn prompt and a drawer; it must be scrubbed first"
        );
    }

    #[test]
    fn the_question_history_is_bounded() {
        let mut pairs: Vec<QaPair> = (0..MAX_QA_PAIRS + 3)
            .map(|i| QaPair {
                question: format!("q{i}"),
                asked_at: "2026-09-02T00:00:00Z".to_string(),
                answer: Some(format!("a{i}")),
                answered_at: Some("2026-09-02T01:00:00Z".to_string()),
            })
            .collect();
        trim_pairs(&mut pairs);
        assert_eq!(pairs.len(), MAX_QA_PAIRS);
        // Newest kept, oldest dropped.
        assert_eq!(
            pairs.last().unwrap().question,
            format!("q{}", MAX_QA_PAIRS + 2)
        );
    }

    #[test]
    fn only_answered_questions_reach_the_turn_prompt() {
        let db = Database::open_in_memory().unwrap();
        upsert_blocked(
            &db,
            &BlockedRecord {
                issue: issue(),
                pairs: vec![
                    QaPair {
                        question: "answered".to_string(),
                        asked_at: "2026-09-01T00:00:00Z".to_string(),
                        answer: Some("yes".to_string()),
                        answered_at: Some("2026-09-01T01:00:00Z".to_string()),
                    },
                    QaPair {
                        question: "still open".to_string(),
                        asked_at: "2026-09-02T00:00:00Z".to_string(),
                        answer: None,
                        answered_at: None,
                    },
                ],
            },
        )
        .unwrap();
        let answers = active_answers(&db, &issue()).unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].question, "answered");
    }
}
