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
//! | Dispatch-time risk classification | [`class_for`] reads a `risk:*` label, else falls back and **fails closed** | Reading an issue's prose to route it |
//! | Composing a strategy redirect | Rung 7 generates it mechanically | It can forbid repeating a failure; it cannot propose a better approach |
//! | Drafting a human escalation question | [`ask_human`](super::blocked::ask_human) takes the text from its caller | Same: naming *what* is unclear is the judgment |
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

use super::blocked::{self, BlockedPoll};
use super::gh::{self, GhRunner};
use super::labels::AgentLabel;
use super::merge::serialize_issue;
use super::queue::{self, QueueConfig, QueuePlan, RepoBacklog, IN_FLIGHT_SCAN_LIMIT};
use super::registry::{self, AgentRegistry, RegistrySnapshot};
use super::run::{self, Dispatcher, IssueBrief, IssueRun, RunConfig};
use super::supervise::{self, SupervisionConfig, SupervisionReport};
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
    pub problems: Vec<TickProblem>,
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
    config: &LeadConfig,
) -> Result<LeadReport, MemoryError> {
    config.validate()?;

    let mut problems: Vec<TickProblem> = Vec::new();

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

    // ── 4. read every backlog and choose ────────────────────────────────
    let backlogs = fetch_backlogs(gh_runner, config, &mut problems);
    let plan = queue::plan_queue(db, &backlogs, &snapshot, &config.queue)?;

    // ── 5. dispatch the top of the queue ────────────────────────────────
    let mut runs = Vec::new();
    if !config.dry_run {
        for queued in plan.dispatch.iter().take(config.max_dispatches_per_tick) {
            match dispatch_one(db, queued, config, dispatcher) {
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

/// Provision a worktree and drive one issue.
fn dispatch_one(
    db: &Database,
    queued: &queue::QueuedIssue,
    config: &LeadConfig,
    dispatcher: &mut dyn Dispatcher,
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
    run_config.dispatch_class = class_for(queued.risk_label.as_deref(), &config.run.dispatch_class);
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
}
