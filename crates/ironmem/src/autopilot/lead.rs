//! The Lead — build-ladder rung 8, and the ladder's last rung.
//!
//! Every rung before this one built a capability the Lead uses and left the
//! *deciding* to a human at a command line: which issue, which class, when.
//! [`lead_tick`] is that human. One tick is one full pass over the world:
//!
//! 1. Read the session registry **once** ([`super::registry::snapshot`]) and
//!    reconcile it against the dispatch-state drawers (rung 7).
//! 2. Poll every `agent:blocked` issue for a human answer (rung 8's
//!    [`super::blocked`]), un-blocking the ones that have been answered
//!    *before* the backlog is read, so an answered issue is workable on the
//!    same tick rather than the next one.
//! 3. Run both supervision checks over every in-flight issue (rung 7), so a
//!    redirect issued this tick reaches this tick's dispatch and an
//!    escalation stops one.
//! 4. Fetch each repo's `agent:ready` backlog and choose
//!    ([`super::queue::plan_queue`]).
//! 5. Dispatch the top of the queue (rung 4's `run_issue`), up to
//!    [`LeadConfig::max_dispatches_per_tick`].
//!
//! # OQ9: is the Lead a Claude session at all?
//!
//! The spec's open question 9 was narrowed by rung 0 and left open pending
//! "rung 4/8 implementation experience", with the explicit caveat that
//! **cross-repo prioritization and thrash detection** were the two
//! responsibilities that could still tip it back toward a persistent Claude
//! session. Rung 7 built thrash detection and found it to be two clocks, a
//! string comparison and a table lookup. This rung builds the other one.
//!
//! The answer, from having written it: **cross-repo prioritization is not
//! judgment-shaped either.** Every input the choice needs is an exact value —
//! a label string, an approval flag, an integer attempt count, a float
//! against a float, a set membership test — and the ordering is a four-key
//! sort over immutable data. There is no step in [`super::queue::plan_queue`]
//! where a language model would be reading anything but its own guess. It is
//! ~40 lines of guards and a `sort_by`.
//!
//! So the recommendation stands and this rung is the evidence for it: a
//! Rust-native mechanical supervisor, unwedgeable by construction, with the
//! genuinely judgment-shaped steps isolated as short one-shot calls. There
//! are exactly three of them, and after rungs 7 and 8 they are smaller and
//! better-bounded than rev 6 assumed:
//!
//! | Judgment-shaped step | Where it is today | Why it is not mechanical |
//! |---|---|---|
//! | Dispatch-time risk classification | [`class_for`] reads a `risk:*` label; [`resolve_class`] asks rung 9's advisor when there is none, and falls back **closed** when it has no answer | Reading an issue's prose to route it |
//! | Composing a strategy redirect | Rung 7 generates the binding text; rung 9 may append a proposed alternative | It can forbid repeating a failure; it cannot propose a better approach |
//! | Drafting a human escalation question | [`notify_escalation`] posts the mechanical notice, with rung 9's drafted question added when there is one | Same: naming *what* is unclear is the judgment |
//!
//! ⟨rung 9⟩ All three are now **built**, in [`super::advise`], and all three
//! are **off unless an operator turns them on**. Nothing above changes when
//! they are off, and — the property the close-out actually rests on —
//! nothing above changes when they are on and *fail*. See
//! `a_failing_advisor_changes_nothing_about_the_tick`.
//!
//! None of the three is on the tick's critical path. A Lead that never makes
//! any of these calls still runs: it dispatches every issue as
//! `unclassified`, which cannot auto-merge (rung 5's `ClassMismatch` holds
//! it for a human), redirects in the mechanical form rung 7 ships, and asks
//! no questions. That is the real close-out — not "the LLM calls turned out
//! to be unnecessary", but **"the loop does not depend on them being
//! available"**, which is the property a cron-restarted supervisor needed.
//!
//! # What one tick will and will not do
//!
//! It will read GitHub, write labels and comments on blocked issues, and
//! spend money on dispatches. It will **not** review or merge: rungs 5 and 6
//! stay behind their own commands this rung. A tick that dispatched, then
//! reviewed, then merged would make the smallest unit of Lead activity the
//! largest possible blast radius, and merge is the one irreversible action
//! in the subsystem. Chaining them is an operator's decision, made per tick,
//! and `--dry-run` exists so the decision can be rehearsed first.

use std::path::PathBuf;

use serde::Serialize;

use super::advise::{self, Advice, AdviceConfig, Advisor};
use super::blocked::{self, BlockedPoll};
use super::gh::{self, GhRunner};
use super::labels::AgentLabel;
use super::merge::serialize_issue;
use super::queue::{self, QueueConfig, QueuePlan, RepoBacklog, IN_FLIGHT_SCAN_LIMIT};
use super::registry::{self, AgentRegistry, RegistrySnapshot};
use super::run::{self, Dispatcher, IssueBrief, IssueRun, RunConfig};
use super::scrub::scrub_and_bound;
use super::supervise::{self, SupervisionAction, SupervisionConfig, SupervisionReport};
use super::worktree;
use super::{validate_repo, IssueRef};
use crate::db::schema::Database;
use crate::error::MemoryError;

/// How many issues one tick will dispatch.
///
/// One, by default, and the default matters: a tick that dispatches its whole
/// queue turns a cron entry into an unbounded fan-out, and the concurrency
/// cap would be the only thing between a misconfigured schedule and every
/// slot filled at once. One per tick makes the *schedule* the rate limit,
/// which is the thing an operator can see and change without redeploying.
pub const DEFAULT_MAX_DISPATCHES_PER_TICK: usize = 1;

/// The label prefix [`class_for`] reads a dispatch-time risk class from.
///
/// No such label exists on this repo today (measured 2026-09-02, same read
/// that established the `priority:*` vocabulary). It is defined here rather
/// than inferred from an issue's text because inferring is the judgment-shaped
/// step OQ9 names, and a keyword heuristic pretending to be that inference
/// would be the worst of both: it would look like classification and route on
/// nothing.
pub const RISK_LABEL_PREFIX: &str = "risk:";

/// Bound on the whole escalation notice.
pub const MAX_NOTICE_CHARS: usize = 20_000;

/// Bound on any one quoted field inside it. Issue and lineage text is
/// scrubbed and bounded on the way to GitHub, like every other comment this
/// subsystem posts.
pub const MAX_NOTICE_FIELD_CHARS: usize = 1_000;

/// How many prior approaches the notice lists, newest first. Bounded to
/// whole approaches rather than truncated mid-sentence — rung 6's
/// exhaustion comment made the same choice for the same reason.
pub const MAX_NOTICE_APPROACHES: usize = 10;

/// One repo the Lead works, and the local checkout its worktrees come from.
///
/// The path is supplied per tick rather than stored on the repo's gate
/// config. Deliberate, and worth revisiting: a checkout location is
/// operator/machine state, not repo policy, and putting it on the approved
/// config would mean re-approving a repo because someone moved a directory.
/// The cost is that a cron entry carries knowledge the database does not.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoTarget {
    pub repo: String,
    /// Local checkout worktrees are cut from.
    pub path: PathBuf,
    /// Committish new issue branches are cut from.
    pub base: String,
}

/// Everything one tick needs.
#[derive(Debug, Clone, PartialEq)]
pub struct LeadConfig {
    pub targets: Vec<RepoTarget>,
    pub queue: QueueConfig,
    /// The `dispatch_class` on this is the **fallback** class, used for any
    /// issue carrying no `risk:*` label. See [`class_for`].
    pub run: RunConfig,
    pub supervision: SupervisionConfig,
    /// Rung 9's three one-shot judgment calls. Disabled by default; a tick
    /// with them disabled is byte-for-byte rung 8's tick.
    pub advice: AdviceConfig,
    pub max_dispatches_per_tick: usize,
    pub worktree_root: PathBuf,
    /// Read everything, decide everything, write nothing and spend nothing.
    pub dry_run: bool,
}

impl LeadConfig {
    pub fn validate(&self) -> Result<(), MemoryError> {
        if self.targets.is_empty() {
            return Err(MemoryError::Validation(
                "at least one repo target is required — a Lead with no repos has no \
                 backlog to work"
                    .into(),
            ));
        }
        let mut seen: Vec<&str> = Vec::new();
        for target in &self.targets {
            validate_repo(&target.repo)?;
            if target.base.trim().is_empty() {
                return Err(MemoryError::Validation(format!(
                    "repo target {} has an empty base committish",
                    target.repo
                )));
            }
            // A repo named twice would be listed twice, planned twice, and
            // could be dispatched twice in one tick against two different
            // checkouts of the same issue.
            if seen.contains(&target.repo.as_str()) {
                return Err(MemoryError::Validation(format!(
                    "repo {} is named more than once",
                    target.repo
                )));
            }
            seen.push(&target.repo);
        }
        if self.max_dispatches_per_tick == 0 {
            return Err(MemoryError::Validation(
                "max_dispatches_per_tick must be at least 1".into(),
            ));
        }
        self.queue.validate()?;
        self.run.validate()?;
        self.supervision.validate()?;
        self.advice.validate()?;
        Ok(())
    }
}

/// The dispatch-time risk class for an issue.
///
/// Takes the `risk:<class>` label [`super::queue::QueuedIssue::risk_label`]
/// read, and otherwise returns `fallback` — normally `"unclassified"`, which
/// **fails closed**:
/// rung 5's `decide_merge` cannot parse it as a [`RiskClass`], so the PR
/// holds at `ClassMismatch` for a human rather than auto-merging. That is the
/// safe direction for a value nobody has actually judged.
///
/// The class is **not** validated against [`super::review::RiskClass`] here.
/// A `risk:banana` label is passed through verbatim and lands at the same
/// `ClassMismatch` hold — one place decides what a class means, and it is the
/// place that decides whether to merge.
pub fn class_for(risk_label: Option<&str>, fallback: &str) -> String {
    risk_label
        .map(str::to_string)
        .unwrap_or_else(|| fallback.to_string())
}

/// One blocked issue this tick looked at.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct BlockedOutcome {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub poll: BlockedPoll,
}

/// Something that went wrong for one repo or one issue without stopping the
/// tick.
///
/// A tick spans repos, and a repo whose `gh` call fails must not take the
/// others down with it — the whole point of a supervisor is that it keeps
/// running. Failures are collected and reported rather than propagated, with
/// one exception: a failure *inside* a dispatch is a real `Err` from
/// `run_issue`, which already banked whatever it spent.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TickProblem {
    pub what: String,
    pub detail: String,
}

/// One full pass of the Lead.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LeadReport {
    pub dry_run: bool,
    pub registry_available: bool,
    pub reconciliation: Vec<supervise::Reconciliation>,
    pub blocked: Vec<BlockedOutcome>,
    pub supervision: Vec<SupervisionReport>,
    pub plan: QueuePlan,
    pub runs: Vec<IssueRun>,
    /// Every rung-9 advisor call this tick made, in the order it made them.
    /// Empty when the advisor is disabled, which is the default.
    pub advice: Vec<Advice>,
    /// Escalated issues a human was told about on the issue itself this
    /// tick. Independent of the advisor: the notice is mechanical, and the
    /// advisor only adds a drafted question to it when it has one.
    pub escalation_notices: Vec<EscalationNotice>,
    pub problems: Vec<TickProblem>,
}

/// One escalated issue a human was notified about.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EscalationNotice {
    #[serde(serialize_with = "serialize_issue")]
    pub issue: IssueRef,
    pub signature: String,
    /// Whether rung 9's advisor supplied a drafted question for the notice.
    pub drafted_question: bool,
}

/// Run one Lead tick.
///
/// See the module doc for the five steps and their order. Every step is
/// bounded and none of them can block: the registry read folds every failure
/// into [`RegistrySnapshot::Unavailable`], `gh` failures are collected into
/// [`LeadReport::problems`], and each dispatch is bounded by the repo's
/// wall-clock timeout (rung 7), its `--max-budget-usd` and its `--max-turns`.
pub fn lead_tick(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    agent_registry: &mut dyn AgentRegistry,
    dispatcher: &mut dyn Dispatcher,
    advisor: &mut dyn Advisor,
    config: &LeadConfig,
) -> Result<LeadReport, MemoryError> {
    config.validate()?;

    let mut problems: Vec<TickProblem> = Vec::new();
    let mut advice: Vec<Advice> = Vec::new();

    // ── 1. one registry read, shared by every step ──────────────────────
    //
    // Once, not per step: two reads inside one tick could disagree, and a
    // reconciliation that adopted a session the queue then treated as dead
    // would be a wrong answer rather than an error.
    let snapshot = registry::snapshot(agent_registry);
    let reconciliation = supervise::reconcile(db, &snapshot)?;

    // ── 2. answered questions, before the backlog is read ───────────────
    let blocked = poll_blocked_issues(db, gh_runner, config, &mut problems);

    // ── 3. supervise what is already in flight ──────────────────────────
    let supervision = supervise_in_flight(db, &snapshot, config, &mut problems)?;

    // ── 3b. act on what supervision decided ⟨rung 9⟩ ────────────────────
    //
    // Two of the three judgment calls live here, and both are *additive*:
    // the redirect already in force is unchanged if no proposal comes back,
    // and the escalation notice is posted whether or not a question was
    // drafted. Ordered after supervision and before the backlog read so a
    // proposal armed this tick reaches this tick's dispatch — the same
    // reason blocked issues are polled first.
    let escalation_notices = act_on_supervision(
        db,
        gh_runner,
        advisor,
        &supervision,
        config,
        &mut advice,
        &mut problems,
    );

    // ── 4. read every backlog and choose ────────────────────────────────
    let backlogs = fetch_backlogs(gh_runner, config, &mut problems);
    let plan = queue::plan_queue(db, &backlogs, &snapshot, &config.queue)?;

    // ── 5. dispatch the top of the queue ────────────────────────────────
    let mut runs = Vec::new();
    if !config.dry_run {
        for queued in plan.dispatch.iter().take(config.max_dispatches_per_tick) {
            match dispatch_one(db, queued, config, dispatcher, advisor, &mut advice) {
                Ok(run) => runs.push(run),
                Err(e) => problems.push(TickProblem {
                    what: format!("dispatch {}", queued.issue.canonical()),
                    detail: e.to_string(),
                }),
            }
        }
    }

    Ok(LeadReport {
        dry_run: config.dry_run,
        registry_available: snapshot.is_available(),
        reconciliation,
        blocked,
        supervision,
        plan,
        runs,
        advice,
        escalation_notices,
        problems,
    })
}

/// Poll every repo's `agent:blocked` issues for human answers.
fn poll_blocked_issues(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    config: &LeadConfig,
    problems: &mut Vec<TickProblem>,
) -> Vec<BlockedOutcome> {
    let mut outcomes = Vec::new();
    for target in &config.targets {
        let listings = match gh::list_labeled_issues(
            gh_runner,
            &target.repo,
            AgentLabel::Blocked.as_str(),
            config.queue.max_issues_per_repo,
        ) {
            Ok(listings) => listings,
            Err(e) => {
                problems.push(TickProblem {
                    what: format!("list {} blocked issues", target.repo),
                    detail: e.to_string(),
                });
                continue;
            }
        };
        for listing in listings {
            let issue = IssueRef::new(target.repo.clone(), listing.number);
            match blocked::poll_answer(db, gh_runner, &issue, config.dry_run) {
                Ok(poll) => outcomes.push(BlockedOutcome { issue, poll }),
                Err(e) => problems.push(TickProblem {
                    what: format!("poll {}", issue.canonical()),
                    detail: e.to_string(),
                }),
            }
        }
    }
    outcomes
}

/// Run both supervision checks over every in-flight issue in a target repo.
///
/// Scoped to the tick's targets rather than to every dispatch-state drawer in
/// the database: an in-flight issue in a repo this Lead was not asked to work
/// belongs to a different invocation's configuration, and supervising it here
/// could issue a redirect or an escalation against work this tick is not
/// responsible for.
fn supervise_in_flight(
    db: &Database,
    snapshot: &RegistrySnapshot,
    config: &LeadConfig,
    problems: &mut Vec<TickProblem>,
) -> Result<Vec<SupervisionReport>, MemoryError> {
    let states = super::dispatch_state::all_dispatch_states(db, IN_FLIGHT_SCAN_LIMIT)?;
    let mut reports = Vec::new();
    for state in states {
        if !config.targets.iter().any(|t| t.repo == state.issue.repo) {
            continue;
        }
        // Supervision *writes* (the supervision record's two clocks, the
        // redirect, the escalation). A rehearsal that armed a redirect would
        // change what the next real tick does, which is not a rehearsal.
        if config.dry_run {
            continue;
        }
        match supervise::supervise_issue(db, &state.issue, snapshot, &config.supervision) {
            Ok(report) => reports.push(report),
            Err(e) => problems.push(TickProblem {
                what: format!("supervise {}", state.issue.canonical()),
                detail: e.to_string(),
            }),
        }
    }
    Ok(reports)
}

/// Act on what supervision decided ⟨rung 9⟩.
///
/// Two things, both **additive to rung 8's behaviour** and both safe to
/// omit:
///
/// 1. A newly-armed strategy redirect may get a model-proposed alternative
///    appended ([`super::advise::advise_strategy_redirect`]). If the advisor
///    is off, refuses, or fails, the redirect rung 7 generated stands
///    unchanged — it is the floor, never the thing being replaced.
/// 2. An escalated issue gets a comment telling a human, **once per
///    signature**. This is not conditional on the advisor: before rung 9 an
///    escalation stopped the work and said so only in a drawer, which is the
///    "reports but does not bind" shape this ladder has now hit four times.
///    The advisor only supplies the drafted question inside the notice.
///
/// Failures are collected, never propagated: a repo whose `gh` call fails
/// must not stop the tick, and neither must an advisor.
///
/// A restart that *also* armed a redirect gets no proposal this tick — only
/// [`SupervisionAction::Redirect`] is matched. Deliberate, and the cheap
/// direction: [`supervise::plan_supervision`] reports a restart when the
/// process is dead, a dead process cannot read a proposal anyway, and the
/// next tick reports `Redirect` and buys one if the issue is still
/// thrashing. The proposal is not lost, only deferred by one tick.
fn act_on_supervision(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    advisor: &mut dyn Advisor,
    supervision: &[SupervisionReport],
    config: &LeadConfig,
    advice: &mut Vec<Advice>,
    problems: &mut Vec<TickProblem>,
) -> Vec<EscalationNotice> {
    let mut notices = Vec::new();
    for report in supervision {
        match &report.action {
            SupervisionAction::Redirect { signature, .. } => {
                if !config.advice.enabled {
                    continue;
                }
                match propose_alternative(db, advisor, report, signature, config) {
                    Ok(Some(a)) => advice.push(a),
                    Ok(None) => {}
                    Err(e) => problems.push(TickProblem {
                        what: format!("propose a redirect for {}", report.issue.canonical()),
                        detail: e.to_string(),
                    }),
                }
            }
            SupervisionAction::Escalate { signature, .. } => {
                match notify_escalation(db, gh_runner, advisor, report, signature, config, advice) {
                    Ok(Some(notice)) => notices.push(notice),
                    Ok(None) => {}
                    Err(e) => problems.push(TickProblem {
                        what: format!("notify escalation on {}", report.issue.canonical()),
                        detail: e.to_string(),
                    }),
                }
            }
            _ => {}
        }
    }
    notices
}

/// Buy one model-proposed alternative for a redirect already in force.
///
/// Returns the [`Advice`] if a call was made at all. The write is
/// [`supervise::set_redirect_proposal`], which refuses a stale or duplicate
/// proposal — so this is idempotent per signature and is *paid for* once,
/// not once per tick.
fn propose_alternative(
    db: &Database,
    advisor: &mut dyn Advisor,
    report: &SupervisionReport,
    signature: &str,
    config: &LeadConfig,
) -> Result<Option<Advice>, MemoryError> {
    // Nothing to add to, or something already added. Checked before the call
    // so a poll loop cannot pay for an answer it would then discard.
    let Some(record) = supervise::get_supervision(db, &report.issue)? else {
        return Ok(None);
    };
    if record.redirect_signature.as_deref() != Some(signature) || record.redirect_proposal.is_some()
    {
        return Ok(None);
    }

    let target = match config.targets.iter().find(|t| t.repo == report.issue.repo) {
        Some(target) => target,
        None => return Ok(None),
    };
    let approaches = attempt_approaches(db, &report.issue)?;
    let advice = advise::advise_strategy_redirect(
        db,
        advisor,
        &target.path,
        &report.issue,
        signature,
        &approaches,
        &config.advice,
    )?;
    if let Some(proposal) = advice.answered() {
        supervise::set_redirect_proposal(db, &report.issue, signature, proposal)?;
    }
    Ok(Some(advice))
}

/// Tell a human, on the issue, that Autopilot has stopped working on it.
///
/// Once per signature: [`supervise::escalation_notified_signature`] gates the
/// comment and [`supervise::mark_escalation_notified`] records it **after**
/// the comment lands. The ordering is rung 6's lesson 15 in the direction
/// that matters here — a mark written first would swallow the one
/// notification the escalation exists to send, and an escalation never
/// self-resolves, so nothing would ever send it again.
#[allow(clippy::too_many_arguments)]
fn notify_escalation(
    db: &Database,
    gh_runner: &mut dyn GhRunner,
    advisor: &mut dyn Advisor,
    report: &SupervisionReport,
    signature: &str,
    config: &LeadConfig,
    advice: &mut Vec<Advice>,
) -> Result<Option<EscalationNotice>, MemoryError> {
    if supervise::escalation_notified_signature(db, &report.issue)?.as_deref() == Some(signature) {
        return Ok(None);
    }

    let approaches = attempt_approaches(db, &report.issue)?;
    let mut question = None;
    if config.advice.enabled {
        if let Some(target) = config.targets.iter().find(|t| t.repo == report.issue.repo) {
            // The brief is read only here, and only once per signature —
            // an in-flight issue is not in any backlog listing, so its text
            // is not already in hand. A failure degrades the prompt rather
            // than skipping the notice.
            let brief = gh::issue_brief(gh_runner, &report.issue).unwrap_or_default();
            let drafted = advise::advise_human_question(
                db,
                advisor,
                &target.path,
                &report.issue,
                &brief.title,
                &brief.body,
                signature,
                &approaches,
                &config.advice,
            )?;
            question = drafted.answered().map(str::to_string);
            advice.push(drafted);
        }
    }

    gh::comment_on_issue(
        gh_runner,
        &report.issue,
        &render_escalation_comment(&report.issue, signature, &approaches, question.as_deref()),
    )?;
    supervise::mark_escalation_notified(db, &report.issue, signature)?;

    Ok(Some(EscalationNotice {
        issue: report.issue.clone(),
        signature: signature.to_string(),
        drafted_question: question.is_some(),
    }))
}

/// The escalation notice posted on the issue.
///
/// Carries [`blocked::AUTOPILOT_COMMENT_MARKER`] like every other comment
/// Autopilot writes. Rung 8's lesson 30: when identity cannot come from
/// authorship, every writer has to stamp, and a fifth renderer that forgot
/// would make this comment look like a human's answer to an open question.
///
/// It does **not** carry `QUESTION_MARKER` even when it contains a drafted
/// question, and it does not flip any label. An escalation is not the
/// `agent:blocked` question round trip: that one resumes on an answer, and
/// this one must not, because the thing being escalated is an approach the
/// supervisor has already proved does not converge. The comment names the
/// command that resumes it instead.
pub fn render_escalation_comment(
    issue: &IssueRef,
    signature: &str,
    approaches: &[String],
    question: Option<&str>,
) -> String {
    let mut body = String::from(blocked::AUTOPILOT_COMMENT_MARKER);
    body.push_str(&format!(
        "\n**Autopilot has stopped working on {issue}.**\n\n\
         Its attempts kept failing the same way, a redirected approach failed the same way \
         too, and it will not start another attempt on this issue without a human.\n\n\
         **The repeated failure:** {signature}\n",
        issue = issue.canonical(),
        signature = scrub_and_bound(signature, MAX_NOTICE_FIELD_CHARS).text,
    ));

    if !approaches.is_empty() {
        body.push_str("\n**Approaches already tried (newest first):**\n");
        for approach in approaches.iter().rev().take(MAX_NOTICE_APPROACHES) {
            body.push_str(&format!(
                "- {}\n",
                scrub_and_bound(approach, MAX_NOTICE_FIELD_CHARS).text
            ));
        }
    }

    if let Some(question) = question {
        body.push_str(&format!(
            "\n**A question that would unblock it:** {}\n\n\
             _Drafted by a model reading the failure history, not by a human._\n",
            scrub_and_bound(question, MAX_NOTICE_FIELD_CHARS).text
        ));
    }

    body.push_str(&format!(
        "\nTo resume it once the cause is understood:\n\n\
         ```\nironmem autopilot supervise {repo} {number} --clear-escalation\n```\n",
        repo = issue.repo,
        number = issue.number,
    ));

    scrub_and_bound(&body, MAX_NOTICE_CHARS).text
}

/// The approaches an issue has already tried, oldest first.
fn attempt_approaches(db: &Database, issue: &IssueRef) -> Result<Vec<String>, MemoryError> {
    Ok(super::lineage::attempts_for_issue(db, issue)?
        .into_iter()
        .filter(|a| !run::is_terminal_summary(&a.approach))
        .map(|a| a.approach)
        .collect())
}

/// Read every target repo's `agent:ready` backlog.
///
/// A repo whose listing fails is **omitted**, and the failure is recorded. It
/// is not represented as an empty backlog: rung 7's lesson 21 again, at repo
/// granularity — an empty listing is a claim that a repo has no work, and the
/// Lead acts on that claim by giving the repo's slots to another repo.
fn fetch_backlogs(
    gh_runner: &mut dyn GhRunner,
    config: &LeadConfig,
    problems: &mut Vec<TickProblem>,
) -> Vec<RepoBacklog> {
    let mut backlogs = Vec::new();
    for target in &config.targets {
        match gh::list_labeled_issues(
            gh_runner,
            &target.repo,
            AgentLabel::Ready.as_str(),
            config.queue.max_issues_per_repo,
        ) {
            Ok(issues) => backlogs.push(RepoBacklog {
                repo: target.repo.clone(),
                issues,
            }),
            Err(e) => problems.push(TickProblem {
                what: format!("list {} ready issues", target.repo),
                detail: e.to_string(),
            }),
        }
    }
    backlogs
}

/// The dispatch class for one queued issue, asking rung 9's advisor when
/// the repo has not already answered.
///
/// **The label always wins, and when there is a label no call is made.**
/// Paying a model to re-derive a fact the repo states outright is the
/// anti-pattern this ladder has avoided by construction; it is also the
/// difference between an advisor that supplements human judgment and one
/// that second-guesses it.
///
/// Every other path returns `config.run.dispatch_class` — the fallback,
/// normally `unclassified`, which **fails closed** at rung 5's
/// `ClassMismatch`. That includes an advisor that is off, refused,
/// unavailable, or that answered `unclear`. An advisor's answer is only ever
/// *better than nothing*, never load-bearing.
pub fn resolve_class(
    db: &Database,
    advisor: &mut dyn Advisor,
    queued: &queue::QueuedIssue,
    repo_path: &std::path::Path,
    config: &LeadConfig,
) -> Result<(String, Option<Advice>), MemoryError> {
    if let Some(label) = queued.risk_label.as_deref() {
        return Ok((class_for(Some(label), &config.run.dispatch_class), None));
    }
    if !config.advice.enabled {
        return Ok((class_for(None, &config.run.dispatch_class), None));
    }

    let advice = advise::advise_risk_class(
        db,
        advisor,
        repo_path,
        &queued.issue,
        &queued.title,
        &queued.body,
        &config.advice,
    )?;
    // `risk_class()` re-parses the answer rather than trusting the output
    // schema, so an answer outside the enum lands on the fallback exactly as
    // a refusal does.
    let class = match advice.risk_class() {
        Some(class) => class.as_str().to_string(),
        None => class_for(None, &config.run.dispatch_class),
    };
    Ok((class, Some(advice)))
}

/// Provision a worktree and drive one issue.
fn dispatch_one(
    db: &Database,
    queued: &queue::QueuedIssue,
    config: &LeadConfig,
    dispatcher: &mut dyn Dispatcher,
    advisor: &mut dyn Advisor,
    advice: &mut Vec<Advice>,
) -> Result<IssueRun, MemoryError> {
    let target = config
        .targets
        .iter()
        .find(|t| t.repo == queued.issue.repo)
        .ok_or_else(|| {
            MemoryError::Validation(format!(
                "no repo target for {} — the queue selected an issue this tick was not \
                 configured to work",
                queued.issue.canonical()
            ))
        })?;

    let mut run_config = config.run.clone();
    let (class, classification) = resolve_class(db, advisor, queued, &target.path, config)?;
    if let Some(classification) = classification {
        advice.push(classification);
    }
    run_config.dispatch_class = class;
    // Validate the per-issue config *before* provisioning, so a class or
    // model that cannot dispatch does not leave a checkout and a branch
    // behind — rung 4's own review finding, in the one place rung 8 repeats
    // the pattern.
    run_config.validate()?;

    let tree = worktree::ensure_worktree(
        &target.path,
        &config.worktree_root,
        &queued.issue,
        &target.base,
    )?;

    run::run_issue(
        db,
        &queued.issue,
        &IssueBrief {
            title: queued.title.clone(),
            body: queued.body.clone(),
        },
        &tree,
        &run_config,
        dispatcher,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::autopilot::advise::testing::{envelope_json, ScriptedAdvisor};
    use crate::autopilot::advise::{AdviceOutput, AdviceStatus};
    use crate::autopilot::dispatch::{DispatchOutcome, DispatchSpec, Verdict};
    use crate::autopilot::gh::testing::ScriptedGh;
    use crate::autopilot::gh::GhOutput;
    use crate::autopilot::registry::RegistryOutput;
    use crate::autopilot::{dispatch_state, gate_config, DispatchState};
    use std::path::Path;

    const REPO: &str = "ironrace/ironmem";

    /// A registry that replays one canned response.
    struct FakeRegistry(Result<RegistryOutput, MemoryError>);

    impl AgentRegistry for FakeRegistry {
        fn list(&mut self) -> Result<RegistryOutput, MemoryError> {
            match &self.0 {
                Ok(out) => Ok(out.clone()),
                Err(e) => Err(MemoryError::NotFound(e.to_string())),
            }
        }
    }

    /// An advisor that fails to launch on every call.
    ///
    /// The default for every test that is not about rung 9, and deliberately
    /// not a no-op stub: if any of them ever starts making an advisor call,
    /// this makes that call fail, and the test must still pass.
    fn no_advisor() -> ScriptedAdvisor {
        ScriptedAdvisor::broken()
    }

    fn empty_registry() -> FakeRegistry {
        FakeRegistry(Ok(RegistryOutput {
            stdout: "[]".to_string(),
            stderr: String::new(),
            success: true,
        }))
    }

    /// A dispatcher that records what it was asked to run and never spends.
    struct RecordingDispatcher {
        seen: Vec<(std::path::PathBuf, DispatchSpec)>,
    }

    impl RecordingDispatcher {
        fn new() -> Self {
            Self { seen: Vec::new() }
        }
    }

    impl Dispatcher for RecordingDispatcher {
        fn dispatch(
            &mut self,
            repo: &Path,
            spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            self.seen.push((repo.to_path_buf(), spec.clone()));
            Ok(DispatchOutcome {
                total_cost_usd: 0.10,
                num_turns: 2,
                duration_ms: 1_000,
                is_error: false,
                session_id: "11111111-2222-3333-4444-555555555555".to_string(),
                verdict: Some(Verdict::Met),
                reason: Some("scripted".to_string()),
                process_success: true,
                timed_out: false,
            })
        }
    }

    /// A dispatcher whose process never comes back cleanly — rung 4's
    /// infrastructure-failure path, which is "paused, not finished" and so
    /// keeps the issue's dispatch-state drawer for a test to read.
    struct FailingDispatcher {
        calls: usize,
    }

    impl FailingDispatcher {
        fn new() -> Self {
            Self { calls: 0 }
        }
    }

    impl Dispatcher for FailingDispatcher {
        fn dispatch(
            &mut self,
            _repo: &Path,
            _spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            self.calls += 1;
            Ok(DispatchOutcome {
                total_cost_usd: 0.0,
                num_turns: 0,
                duration_ms: 10,
                is_error: true,
                session_id: "11111111-2222-3333-4444-555555555555".to_string(),
                verdict: None,
                reason: Some("scripted infrastructure failure".to_string()),
                process_success: false,
                timed_out: false,
            })
        }
    }

    /// A dispatcher that must never be called.
    struct RefusingDispatcher;

    impl Dispatcher for RefusingDispatcher {
        fn dispatch(
            &mut self,
            _repo: &Path,
            spec: &DispatchSpec,
        ) -> Result<DispatchOutcome, MemoryError> {
            panic!("this tick must not dispatch, but it dispatched {spec:?}");
        }
    }

    fn issue_list_json(entries: &[(u64, &[&str])]) -> String {
        let items: Vec<String> = entries
            .iter()
            .map(|(number, labels)| {
                let labels: Vec<String> = labels
                    .iter()
                    .map(|l| format!(r#"{{"name":"{l}"}}"#))
                    .collect();
                format!(
                    r#"{{"number":{number},"title":"issue {number}","body":"body",
                        "labels":[{}],"updatedAt":"2026-09-02T00:00:00Z"}}"#,
                    labels.join(",")
                )
            })
            .collect();
        format!("[{}]", items.join(","))
    }

    fn approved_db() -> Database {
        let db = Database::open_in_memory().unwrap();
        gate_config::propose_gate_config(
            &db,
            REPO,
            vec!["cargo test --workspace".to_string()],
            Vec::new(),
        )
        .unwrap();
        gate_config::approve_gate_config(&db, REPO).unwrap();
        db
    }

    /// A real git checkout, so `ensure_worktree` has something to cut from.
    fn git_repo(dir: &Path) {
        let run = |args: &[&str]| {
            let status = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .expect("git must be available");
            assert!(
                status.status.success(),
                "git {args:?} failed: {}",
                String::from_utf8_lossy(&status.stderr)
            );
        };
        run(&["init", "--initial-branch=main"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        std::fs::write(dir.join("README.md"), "hi").unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
    }

    struct Fixture {
        _root: tempfile::TempDir,
        repo_path: PathBuf,
        worktree_root: PathBuf,
    }

    fn fixture() -> Fixture {
        let root = tempfile::tempdir().unwrap();
        let repo_path = root.path().join("checkout");
        std::fs::create_dir_all(&repo_path).unwrap();
        git_repo(&repo_path);
        let worktree_root = root.path().join("worktrees");
        Fixture {
            _root: root,
            repo_path,
            worktree_root,
        }
    }

    fn config(fixture: &Fixture) -> LeadConfig {
        LeadConfig {
            targets: vec![RepoTarget {
                repo: REPO.to_string(),
                path: fixture.repo_path.clone(),
                base: "HEAD".to_string(),
            }],
            queue: QueueConfig::default(),
            run: RunConfig::new("claude-sonnet-5", "unclassified"),
            supervision: SupervisionConfig::default(),
            // Off, as it ships. Every test below therefore exercises rung
            // 8's tick, and the rung-9 tests turn it on explicitly.
            advice: AdviceConfig::default(),
            max_dispatches_per_tick: DEFAULT_MAX_DISPATCHES_PER_TICK,
            worktree_root: fixture.worktree_root.clone(),
            dry_run: false,
        }
    }

    // ── configuration ───────────────────────────────────────────────────

    #[test]
    fn a_lead_with_no_repos_is_refused() {
        let f = fixture();
        let mut cfg = config(&f);
        cfg.targets.clear();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn the_same_repo_named_twice_is_refused() {
        // Otherwise one issue could be planned twice in a tick and dispatched
        // against two different checkouts.
        let f = fixture();
        let mut cfg = config(&f);
        let dup = cfg.targets[0].clone();
        cfg.targets.push(dup);
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn a_zero_dispatch_tick_is_refused() {
        let f = fixture();
        let mut cfg = config(&f);
        cfg.max_dispatches_per_tick = 0;
        assert!(cfg.validate().is_err());
    }

    // ── risk classification ─────────────────────────────────────────────

    #[test]
    fn an_unlabeled_issue_dispatches_as_the_fallback_class() {
        assert_eq!(class_for(None, "unclassified"), "unclassified");
        assert_eq!(class_for(Some("logic"), "unclassified"), "logic");
    }

    #[test]
    fn the_fallback_class_cannot_be_auto_merged() {
        // The whole reason "unclassified" is a safe default: rung 5's
        // `decide_merge` cannot parse it, so the PR holds for a human.
        use crate::autopilot::review::RiskClass;
        assert!(RiskClass::from_schema_str(&class_for(None, "unclassified")).is_none());
    }

    // ── the tick ────────────────────────────────────────────────────────

    #[test]
    fn a_tick_polls_blocked_issues_before_it_reads_the_ready_backlog() {
        // An answered issue must be workable on the same tick, not the next.
        let db = approved_db();
        let f = fixture();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")), // blocked listing
            Ok(GhOutput::ok("[]")), // ready listing
        ]);
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut no_advisor(),
            &config(&f),
        )
        .unwrap();

        assert!(gh_runner.seen[0].contains(&AgentLabel::Blocked.as_str().to_string()));
        assert!(gh_runner.seen[1].contains(&AgentLabel::Ready.as_str().to_string()));
        assert!(report.problems.is_empty());
        assert!(report.plan.is_empty());
    }

    #[test]
    fn a_tick_dispatches_the_top_of_the_queue_and_only_that() {
        let db = approved_db();
        let f = fixture();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::ok(&issue_list_json(&[
                (7, &["agent:ready"]),
                (3, &["agent:ready", "priority:high"]),
            ]))),
        ]);
        let mut dispatcher = RecordingDispatcher::new();
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut dispatcher,
            &mut no_advisor(),
            &config(&f),
        )
        .unwrap();

        assert_eq!(report.plan.dispatch.len(), 2, "both issues are eligible");
        assert_eq!(report.runs.len(), 1, "but only one is dispatched per tick");
        assert_eq!(report.runs[0].issue.number, 3, "the high-priority one");
        assert_eq!(dispatcher.seen.len(), 1);
        assert!(report.problems.is_empty());
    }

    #[test]
    fn the_dispatched_class_comes_from_the_issues_risk_label() {
        // Asserted against the dispatch-state drawer — the value the
        // Reviewer later compares its own classification against — rather
        // than against the fact that a dispatch happened. Rung 7's lesson
        // 27's sibling: a test that only proves the code ran is a comment.
        //
        // The dispatcher returns an infrastructure failure so the run is
        // "paused, not finished" and keeps its drawer; a met run clears it.
        let db = approved_db();
        let f = fixture();
        let mut cfg = config(&f);
        cfg.run.max_consecutive_infrastructure_failures = 1;
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::ok(&issue_list_json(&[(
                3,
                &["agent:ready", "risk:documentation"],
            )]))),
        ]);
        let mut dispatcher = FailingDispatcher::new();
        lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut dispatcher,
            &mut no_advisor(),
            &cfg,
        )
        .unwrap();

        let state = dispatch_state::get_dispatch_state(&db, &IssueRef::new(REPO, 3))
            .unwrap()
            .expect("an infrastructure failure keeps the dispatch state");
        assert_eq!(state.dispatch_class, "documentation");
    }

    #[test]
    fn an_issue_with_no_risk_label_dispatches_as_the_configured_fallback() {
        let db = approved_db();
        let f = fixture();
        let mut cfg = config(&f);
        cfg.run.max_consecutive_infrastructure_failures = 1;
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::ok(&issue_list_json(&[(3, &["agent:ready"])]))),
        ]);
        let mut dispatcher = FailingDispatcher::new();
        lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut dispatcher,
            &mut no_advisor(),
            &cfg,
        )
        .unwrap();

        let state = dispatch_state::get_dispatch_state(&db, &IssueRef::new(REPO, 3))
            .unwrap()
            .unwrap();
        assert_eq!(state.dispatch_class, "unclassified");
    }

    #[test]
    fn a_dry_run_spends_nothing_and_writes_nothing() {
        let db = approved_db();
        let f = fixture();
        let mut cfg = config(&f);
        cfg.dry_run = true;
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::ok(&issue_list_json(&[(3, &["agent:ready"])]))),
        ]);
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut no_advisor(),
            &cfg,
        )
        .unwrap();

        assert!(report.dry_run);
        assert_eq!(report.plan.dispatch.len(), 1, "the plan is still computed");
        assert!(report.runs.is_empty(), "but nothing runs");
        assert!(
            dispatch_state::get_dispatch_state(&db, &IssueRef::new(REPO, 3))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn a_repo_whose_listing_fails_is_omitted_not_reported_as_empty() {
        // Rung 7's lesson 21 at repo granularity: an empty listing is a claim
        // that a repo has no work, and the Lead acts on it by giving the
        // repo's slots away.
        let db = approved_db();
        let f = fixture();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::failed("", "HTTP 403: rate limited")),
        ]);
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut no_advisor(),
            &config(&f),
        )
        .unwrap();

        assert!(report.plan.is_empty());
        assert!(
            report.plan.deferred.is_empty(),
            "an unreadable repo yields no issues at all, deferred or otherwise"
        );
        assert_eq!(report.problems.len(), 1);
        assert!(report.problems[0].what.contains("ready issues"));
    }

    #[test]
    fn one_repos_failure_does_not_stop_another_repos_work() {
        let db = approved_db();
        gate_config::propose_gate_config(
            &db,
            "ironrace/other",
            vec!["cargo test".to_string()],
            Vec::new(),
        )
        .unwrap();
        gate_config::approve_gate_config(&db, "ironrace/other").unwrap();

        let f = fixture();
        let mut cfg = config(&f);
        cfg.targets.push(RepoTarget {
            repo: "ironrace/other".to_string(),
            path: f.repo_path.clone(),
            base: "HEAD".to_string(),
        });

        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::failed("", "HTTP 403")), // repo 1 blocked listing
            Ok(GhOutput::ok("[]")),               // repo 2 blocked listing
            Ok(GhOutput::failed("", "HTTP 403")), // repo 1 ready listing
            Ok(GhOutput::ok(&issue_list_json(&[(5, &["agent:ready"])]))),
        ]);
        let mut dispatcher = RecordingDispatcher::new();
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut dispatcher,
            &mut no_advisor(),
            &cfg,
        )
        .unwrap();

        assert_eq!(report.runs.len(), 1);
        assert_eq!(report.runs[0].issue.repo, "ironrace/other");
        assert_eq!(report.problems.len(), 2);
    }

    #[test]
    fn an_unreadable_registry_does_not_stop_the_tick() {
        let db = approved_db();
        let f = fixture();
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::ok(&issue_list_json(&[(3, &["agent:ready"])]))),
        ]);
        let mut registry = FakeRegistry(Err(MemoryError::NotFound("no claude".into())));
        let mut dispatcher = RecordingDispatcher::new();
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut registry,
            &mut dispatcher,
            &mut no_advisor(),
            &config(&f),
        )
        .unwrap();

        assert!(!report.registry_available);
        assert_eq!(
            report.runs.len(),
            1,
            "an issue with no session has nothing to collide with"
        );
    }

    #[test]
    fn an_in_flight_issue_in_an_untargeted_repo_is_not_supervised() {
        // Its redirect and escalation budget belong to whichever invocation
        // is configured for that repo.
        let db = approved_db();
        let f = fixture();
        dispatch_state::upsert_dispatch_state(
            &db,
            &DispatchState {
                issue: IssueRef::new("ironrace/elsewhere", 1),
                worktree_path: "/tmp/wt".to_string(),
                ic_session_name: "ic-ironrace-elsewhere-1".to_string(),
                dispatch_class: "logic".to_string(),
                attempt_n: 1,
                state: "dispatching".to_string(),
                started_at: "2026-09-02T00:00:00Z".to_string(),
                session_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                turn_n: 1,
                session_claimed: true,
            },
        )
        .unwrap();

        let mut gh_runner = ScriptedGh::new(vec![Ok(GhOutput::ok("[]")), Ok(GhOutput::ok("[]"))]);
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut no_advisor(),
            &config(&f),
        )
        .unwrap();
        assert!(report.supervision.is_empty());
    }

    #[test]
    fn an_answered_blocked_issue_is_unblocked_by_the_tick() {
        let db = approved_db();
        let f = fixture();
        let question = format!(
            "{}\nWhich schema?",
            crate::autopilot::blocked::QUESTION_MARKER
        );
        let mut gh_runner = ScriptedGh::new(vec![
            // blocked listing
            Ok(GhOutput::ok(&issue_list_json(&[(4, &["agent:blocked"])]))),
            // poll: labels, comments, ensure label, edit
            Ok(GhOutput::ok(r#"{"labels":[{"name":"agent:blocked"}]}"#)),
            Ok(GhOutput::ok(&format!(
                r#"{{"comments":[
                    {{"author":{{"login":"bot"}},"body":{},"createdAt":"2026-09-02T00:00:00Z"}},
                    {{"author":{{"login":"jeff"}},"body":"Use SQLite.","createdAt":"2026-09-02T01:00:00Z"}}
                ]}}"#,
                serde_json::to_string(&question).unwrap()
            ))),
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok("")),
            // ready listing
            Ok(GhOutput::ok("[]")),
        ]);
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut no_advisor(),
            &config(&f),
        )
        .unwrap();

        assert_eq!(report.blocked.len(), 1);
        assert!(matches!(
            report.blocked[0].poll,
            crate::autopilot::blocked::BlockedPoll::Answered { .. }
        ));
        // Delivered, not merely observed.
        let answers =
            crate::autopilot::blocked::active_answers(&db, &IssueRef::new(REPO, 4)).unwrap();
        assert_eq!(answers.len(), 1);
    }

    // ── rung 9: the three judgment calls ────────────────────────────────

    fn advising(structured: &str) -> ScriptedAdvisor {
        ScriptedAdvisor::new(vec![Ok(AdviceOutput {
            stdout: envelope_json(structured, 0.02),
            stderr: String::new(),
            success: true,
        })])
    }

    fn with_advisor(f: &Fixture) -> LeadConfig {
        let mut cfg = config(f);
        cfg.advice.enabled = true;
        cfg
    }

    /// One `agent:ready` issue, no `risk:*` label.
    fn one_unlabeled_issue() -> ScriptedGh {
        ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::ok(&issue_list_json(&[(3, &["agent:ready"])]))),
        ])
    }

    #[test]
    fn a_failing_advisor_changes_nothing_about_the_tick() {
        // **The rung's central claim.** OQ9's close-out does not rest on the
        // three calls being good; it rests on the loop not depending on them
        // being available. So a tick with the advisor on and broken must
        // dispatch the same issue, with the same class, and report no
        // problem — identically to a tick with the advisor off.
        let run_tick = |enabled: bool| {
            let db = approved_db();
            let f = fixture();
            let mut cfg = config(&f);
            cfg.advice.enabled = enabled;
            cfg.run.max_consecutive_infrastructure_failures = 1;
            let mut dispatcher = FailingDispatcher::new();
            let report = lead_tick(
                &db,
                &mut one_unlabeled_issue(),
                &mut empty_registry(),
                &mut dispatcher,
                &mut ScriptedAdvisor::broken(),
                &cfg,
            )
            .unwrap();
            let state = dispatch_state::get_dispatch_state(&db, &IssueRef::new(REPO, 3))
                .unwrap()
                .expect("the issue was dispatched");
            (report, state.dispatch_class)
        };

        let (off, class_off) = run_tick(false);
        let (on, class_on) = run_tick(true);

        assert_eq!(class_off, "unclassified");
        assert_eq!(class_on, class_off, "a broken advisor changes no class");
        assert_eq!(on.runs.len(), off.runs.len(), "the same work happened");
        assert!(
            on.problems.is_empty() && off.problems.is_empty(),
            "an advisor that cannot launch is not a problem with the tick"
        );
        // It is reported, though — the operator can see it failed.
        assert_eq!(off.advice.len(), 0, "disabled means no call at all");
        assert_eq!(on.advice.len(), 1);
        assert!(matches!(
            on.advice[0].status,
            AdviceStatus::Unavailable { .. }
        ));
    }

    #[test]
    fn an_issue_with_a_risk_label_never_costs_an_advisor_call() {
        // The label is the answer. Paying a model to re-derive a fact the
        // repo states outright is the anti-pattern, and it is also the line
        // between supplementing human judgment and second-guessing it.
        let db = approved_db();
        let f = fixture();
        let mut cfg = with_advisor(&f);
        cfg.run.max_consecutive_infrastructure_failures = 1;
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::ok(&issue_list_json(&[(
                3,
                &["agent:ready", "risk:logic"],
            )]))),
        ]);
        let mut advisor = ScriptedAdvisor::broken();
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut FailingDispatcher::new(),
            &mut advisor,
            &cfg,
        )
        .unwrap();

        assert!(advisor.seen.is_empty(), "nothing was asked");
        assert!(report.advice.is_empty());
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &IssueRef::new(REPO, 3))
                .unwrap()
                .unwrap()
                .dispatch_class,
            "logic"
        );
    }

    #[test]
    fn an_advised_class_reaches_the_dispatch_state_drawer() {
        // Asserted against the value the Reviewer later compares its own
        // classification with, not against the fact that a call was made.
        let db = approved_db();
        let f = fixture();
        let mut cfg = with_advisor(&f);
        cfg.run.max_consecutive_infrastructure_failures = 1;
        let mut advisor = advising(r#"{"risk_class":"documentation","reason":"README only"}"#);
        let report = lead_tick(
            &db,
            &mut one_unlabeled_issue(),
            &mut empty_registry(),
            &mut FailingDispatcher::new(),
            &mut advisor,
            &cfg,
        )
        .unwrap();

        assert_eq!(report.advice.len(), 1);
        assert_eq!(report.advice[0].status, AdviceStatus::Answered);
        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &IssueRef::new(REPO, 3))
                .unwrap()
                .unwrap()
                .dispatch_class,
            "documentation"
        );
    }

    #[test]
    fn an_unclear_class_falls_back_to_the_class_that_cannot_auto_merge() {
        let db = approved_db();
        let f = fixture();
        let mut cfg = with_advisor(&f);
        cfg.run.max_consecutive_infrastructure_failures = 1;
        let mut advisor = advising(r#"{"risk_class":"unclear","reason":"two sentences"}"#);
        lead_tick(
            &db,
            &mut one_unlabeled_issue(),
            &mut empty_registry(),
            &mut FailingDispatcher::new(),
            &mut advisor,
            &cfg,
        )
        .unwrap();

        assert_eq!(
            dispatch_state::get_dispatch_state(&db, &IssueRef::new(REPO, 3))
                .unwrap()
                .unwrap()
                .dispatch_class,
            "unclassified"
        );
    }

    #[test]
    fn a_dry_run_asks_the_advisor_nothing() {
        // A rehearsal must not spend. Classification happens at dispatch and
        // supervision is skipped outright, so a dry run makes no call at
        // all — which also means it cannot show the class it would choose.
        let db = approved_db();
        let f = fixture();
        let mut cfg = with_advisor(&f);
        cfg.dry_run = true;
        let mut advisor = ScriptedAdvisor::broken();
        let report = lead_tick(
            &db,
            &mut one_unlabeled_issue(),
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut advisor,
            &cfg,
        )
        .unwrap();

        assert!(advisor.seen.is_empty());
        assert!(report.advice.is_empty());
        assert!(report.escalation_notices.is_empty());
    }

    // ── the escalation notice ───────────────────────────────────────────

    /// An in-flight issue that has thrashed one signature past a redirect
    /// that was actually delivered, so supervision escalates it.
    fn escalated_issue(db: &Database, f: &Fixture) -> IssueRef {
        use crate::autopilot::lineage::{self, AttemptOutcome, AttemptRecord};

        let issue = IssueRef::new(REPO, 3);
        dispatch_state::upsert_dispatch_state(
            db,
            &DispatchState {
                issue: issue.clone(),
                worktree_path: f.repo_path.display().to_string(),
                ic_session_name: crate::autopilot::dispatch::ic_name(&issue),
                dispatch_class: "unclassified".to_string(),
                attempt_n: 4,
                state: "dispatching".to_string(),
                started_at: chrono::Utc::now().to_rfc3339(),
                session_claimed: true,
                session_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                turn_n: 0,
            },
        )
        .unwrap();
        for n in 1..=4u32 {
            lineage::record_attempt(
                db,
                &AttemptRecord {
                    issue: issue.clone(),
                    attempt_n: n,
                    approach: format!("attempt {n}: rewrote the parser"),
                    verdict: AttemptOutcome::Failed,
                    why_failed: Some("assertion failed: left == right".to_string()),
                    commit_sha: None,
                },
            )
            .unwrap();
        }
        // A redirect already issued and already delivered: escalation
        // requires proof the IC actually ran with it.
        supervise::upsert_supervision(
            db,
            &supervise::SupervisionRecord {
                issue: issue.clone(),
                fingerprint: String::new(),
                progress_observed_at: chrono::Utc::now().to_rfc3339(),
                first_absent_at: None,
                last_checked_at: chrono::Utc::now().to_rfc3339(),
                active_redirect: Some("do not repeat it".to_string()),
                redirect_signature: Some("assertion failed: left == right".to_string()),
                redirect_issued_after_attempts: Some(2),
                escalated_signature: None,
                redirect_proposal: None,
                escalation_notified_signature: None,
            },
        )
        .unwrap();
        issue
    }

    #[test]
    fn an_escalation_is_reported_to_a_human_even_with_no_advisor() {
        // Before rung 9 an escalation stopped the work and said so only in a
        // drawer — the "reports but does not bind" shape this ladder keeps
        // hitting. The notice is mechanical, so it does not depend on the
        // advisor at all.
        let db = approved_db();
        let f = fixture();
        let issue = escalated_issue(&db, &f);
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")), // blocked listing
            Ok(GhOutput::ok("")),   // the escalation comment
            Ok(GhOutput::ok("[]")), // ready listing
        ]);
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut no_advisor(),
            &config(&f),
        )
        .unwrap();

        assert_eq!(report.escalation_notices.len(), 1);
        assert!(!report.escalation_notices[0].drafted_question);
        assert!(report.problems.is_empty());

        let comment = gh_runner
            .seen
            .iter()
            .find(|argv| argv.contains(&"comment".to_string()))
            .expect("a comment was posted");
        let body = comment.last().unwrap();
        assert!(body.contains("--clear-escalation"), "it names the way out");
        assert!(body.contains("assertion failed"));
        assert_eq!(
            supervise::escalation_notified_signature(&db, &issue)
                .unwrap()
                .as_deref(),
            Some("assertion failed: left == right")
        );
    }

    #[test]
    fn a_human_is_told_once_per_signature_not_once_per_tick() {
        // An escalation never self-resolves, so the naive "notify the human"
        // is "bury the human" — rung 6's lesson 19, in the one place rung 9
        // adds a new comment writer.
        let db = approved_db();
        let f = fixture();
        escalated_issue(&db, &f);
        for tick in 0..2 {
            let mut gh_runner = ScriptedGh::new(if tick == 0 {
                vec![
                    Ok(GhOutput::ok("[]")),
                    Ok(GhOutput::ok("")),
                    Ok(GhOutput::ok("[]")),
                ]
            } else {
                // No comment call is scripted: a second one would run the
                // script dry and fail the tick.
                vec![Ok(GhOutput::ok("[]")), Ok(GhOutput::ok("[]"))]
            });
            let report = lead_tick(
                &db,
                &mut gh_runner,
                &mut empty_registry(),
                &mut RefusingDispatcher,
                &mut no_advisor(),
                &config(&f),
            )
            .unwrap();
            assert!(
                report.problems.is_empty(),
                "tick {tick}: {:?}",
                report.problems
            );
            assert_eq!(
                report.escalation_notices.len(),
                if tick == 0 { 1 } else { 0 }
            );
        }
    }

    #[test]
    fn a_drafted_question_is_added_to_the_notice_when_the_advisor_has_one() {
        let db = approved_db();
        let f = fixture();
        escalated_issue(&db, &f);
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")), // blocked listing
            Ok(GhOutput::ok(
                r#"{"title":"Migrate the table","body":"Move the rows"}"#,
            )), // issue brief
            Ok(GhOutput::ok("")),   // the escalation comment
            Ok(GhOutput::ok("[]")), // ready listing
        ]);
        let mut advisor = advising(
            r#"{"verdict":"question","question":"Should soft-deleted rows be migrated?","reason":"unstated"}"#,
        );
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut advisor,
            &with_advisor(&f),
        )
        .unwrap();

        assert_eq!(report.escalation_notices.len(), 1);
        assert!(report.escalation_notices[0].drafted_question);
        let body = gh_runner
            .seen
            .iter()
            .find(|argv| argv.contains(&"comment".to_string()))
            .unwrap()
            .last()
            .unwrap();
        assert!(body.contains("Should soft-deleted rows be migrated?"));
        assert!(
            body.contains("not by a human"),
            "a drafted question must not read as a human's"
        );
    }

    #[test]
    fn an_escalation_notice_is_posted_even_when_the_drafting_call_fails() {
        // The notice is the thing a human needs. Losing it because an
        // optional call failed would make rung 9 a regression.
        let db = approved_db();
        let f = fixture();
        escalated_issue(&db, &f);
        let mut gh_runner = ScriptedGh::new(vec![
            Ok(GhOutput::ok("[]")),
            Ok(GhOutput::failed("", "HTTP 500")), // the brief read fails too
            Ok(GhOutput::ok("")),
            Ok(GhOutput::ok("[]")),
        ]);
        let report = lead_tick(
            &db,
            &mut gh_runner,
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut ScriptedAdvisor::broken(),
            &with_advisor(&f),
        )
        .unwrap();

        assert_eq!(report.escalation_notices.len(), 1);
        assert!(!report.escalation_notices[0].drafted_question);
    }

    // ── the redirect proposal ───────────────────────────────────────────

    #[test]
    fn a_proposal_is_attached_to_a_redirect_the_tick_just_armed() {
        let db = approved_db();
        let f = fixture();
        let issue = IssueRef::new(REPO, 3);
        {
            use crate::autopilot::lineage::{self, AttemptOutcome, AttemptRecord};
            dispatch_state::upsert_dispatch_state(
                &db,
                &DispatchState {
                    issue: issue.clone(),
                    worktree_path: f.repo_path.display().to_string(),
                    ic_session_name: crate::autopilot::dispatch::ic_name(&issue),
                    dispatch_class: "unclassified".to_string(),
                    attempt_n: 3,
                    state: "dispatching".to_string(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    session_claimed: true,
                    session_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                    turn_n: 0,
                },
            )
            .unwrap();
            for n in 1..=3u32 {
                lineage::record_attempt(
                    &db,
                    &AttemptRecord {
                        issue: issue.clone(),
                        attempt_n: n,
                        approach: format!("attempt {n}"),
                        verdict: AttemptOutcome::Failed,
                        why_failed: Some("the same failure".to_string()),
                        commit_sha: None,
                    },
                )
                .unwrap();
            }
        }
        let mut advisor = advising(
            r#"{"verdict":"proposal","proposal":"The fixture is wrong, not the code.","reason":"three identical failures"}"#,
        );
        let report = lead_tick(
            &db,
            &mut ScriptedGh::new(vec![Ok(GhOutput::ok("[]")), Ok(GhOutput::ok("[]"))]),
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut advisor,
            &with_advisor(&f),
        )
        .unwrap();

        assert_eq!(report.advice.len(), 1);
        assert_eq!(report.advice[0].status, AdviceStatus::Answered);

        let redirect = supervise::active_redirect(&db, &issue).unwrap().unwrap();
        assert!(
            redirect.contains("Do NOT repeat it"),
            "the mechanical floor survives"
        );
        assert!(redirect.contains("The fixture is wrong"));
    }

    #[test]
    fn a_failing_proposal_leaves_the_mechanical_redirect_exactly_as_rung_7_wrote_it() {
        let db = approved_db();
        let f = fixture();
        let issue = IssueRef::new(REPO, 3);
        {
            use crate::autopilot::lineage::{self, AttemptOutcome, AttemptRecord};
            dispatch_state::upsert_dispatch_state(
                &db,
                &DispatchState {
                    issue: issue.clone(),
                    worktree_path: f.repo_path.display().to_string(),
                    ic_session_name: crate::autopilot::dispatch::ic_name(&issue),
                    dispatch_class: "unclassified".to_string(),
                    attempt_n: 3,
                    state: "dispatching".to_string(),
                    started_at: chrono::Utc::now().to_rfc3339(),
                    session_claimed: true,
                    session_uuid: "11111111-2222-3333-4444-555555555555".to_string(),
                    turn_n: 0,
                },
            )
            .unwrap();
            for n in 1..=3u32 {
                lineage::record_attempt(
                    &db,
                    &AttemptRecord {
                        issue: issue.clone(),
                        attempt_n: n,
                        approach: format!("attempt {n}"),
                        verdict: AttemptOutcome::Failed,
                        why_failed: Some("the same failure".to_string()),
                        commit_sha: None,
                    },
                )
                .unwrap();
            }
        }
        let report = lead_tick(
            &db,
            &mut ScriptedGh::new(vec![Ok(GhOutput::ok("[]")), Ok(GhOutput::ok("[]"))]),
            &mut empty_registry(),
            &mut RefusingDispatcher,
            &mut ScriptedAdvisor::broken(),
            &with_advisor(&f),
        )
        .unwrap();

        assert!(report.problems.is_empty());
        let redirect = supervise::active_redirect(&db, &issue).unwrap().unwrap();
        assert_eq!(
            redirect,
            supervise::redirect_text("the same failure", 3),
            "unchanged, byte for byte"
        );
    }

    // ── the notice itself ───────────────────────────────────────────────

    #[test]
    fn the_escalation_comment_is_stamped_like_every_other_autopilot_comment() {
        // Rung 8's lesson 30: half a marker scheme is worse than none. An
        // unstamped fifth renderer would make this comment readable as a
        // human's answer to an open question.
        let body = render_escalation_comment(
            &IssueRef::new(REPO, 3),
            "assertion failed",
            &["rewrote the parser".to_string()],
            Some("Which fixture is authoritative?"),
        );
        assert!(body.starts_with(blocked::AUTOPILOT_COMMENT_MARKER));
        assert!(
            !body.contains(blocked::QUESTION_MARKER),
            "an escalation is not the agent:blocked question round trip — an \
             answer must not resume an approach already proved not to converge"
        );
        assert!(body.contains("ironmem autopilot supervise"));
    }

    #[test]
    fn the_escalation_comment_is_scrubbed_and_bounded() {
        let secret = "token sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKK";
        let body = render_escalation_comment(
            &IssueRef::new(REPO, 3),
            secret,
            &[format!("x{}", "y".repeat(50_000))],
            None,
        );
        assert!(!body.contains("sk-ant-api03-AAAABBBBCCCCDDDDEEEEFFFFGGGGHHHHIIIIJJJJKKKK"));
        assert!(body.chars().count() <= MAX_NOTICE_CHARS);
    }
}
