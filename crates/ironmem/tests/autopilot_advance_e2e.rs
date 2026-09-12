//! Rung 10 end-to-end: the loop closing, against real everything.
//!
//! A real SQLite database, a real git checkout with a real worktree on the
//! issue's branch, the real `GhCli` and `CodexReviewer` runners, and the real
//! argv each of them builds. Only the two binaries are stubbed — behind an
//! **asserted PATH guard**, so a stub that fails to shadow the real thing
//! fails the test rather than quietly spending money.
//!
//! Nothing here can reach the network or an API. **$0.00.**

use std::path::{Path, PathBuf};

use ironmem::autopilot::advance::{
    advance_pass, AdvanceConfig, AdvanceStep, NoPrReason, SkipReason, Stall,
    DEFAULT_MAX_ADVANCES_PER_PASS,
};
use ironmem::autopilot::gh::{GhCli, MergeStrategy};
use ironmem::autopilot::lead::RepoTarget;
use ironmem::autopilot::lineage::{self, AttemptOutcome, IssueStatus};
use ironmem::autopilot::merge::MergeOutcome;
use ironmem::autopilot::review::CodexReviewer;
use ironmem::autopilot::worktree::{self, WorktreeRemoval};
use ironmem::autopilot::{dispatch_state, gate_config, DispatchState, IssueRef};
use ironmem::db::schema::Database;

const REPO: &str = "owner/repo";

/// `current_dir` is **not** a repository selector: git reads `GIT_DIR` first,
/// so an inherited one aims these calls at whatever repo the environment
/// names. This suite is threaded and another module's tests set `GIT_DIR`
/// process-wide, so the variables are stripped here the way
/// `autopilot::worktree`'s own `git_command` strips them.
fn git(dir: &Path, args: &[&str]) {
    let mut command = std::process::Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    let out = command
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git must be available");
    assert!(
        out.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn issue() -> IssueRef {
    IssueRef::new(REPO, 7)
}

/// Write an executable stub and return its path.
fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The whole rung in one sequential test.
///
/// One test rather than eight, because it installs stubs on `PATH` for the
/// whole process: two of these running concurrently would each see the
/// other's fixtures.
#[test]
#[cfg(unix)]
fn the_loop_closes_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let stubs = root.path().join("stubs");
    let fixtures = root.path().join("fixtures");
    let checkout = root.path().join("checkout");
    let worktree_root = root.path().join("worktrees");
    let log = root.path().join("calls.log");
    std::fs::create_dir_all(&stubs).unwrap();
    std::fs::create_dir_all(&fixtures).unwrap();
    std::fs::create_dir_all(&checkout).unwrap();
    std::fs::create_dir_all(&worktree_root).unwrap();

    // ── stubs ───────────────────────────────────────────────────────────
    stub(
        &stubs,
        "gh",
        r#"#!/bin/bash
echo "gh $*" >> "$AP_LOG"
case "$1 $2" in
  "issue list") cat "$AP_FIXTURES/issue_list.json"; exit 0 ;;
  # `--state open` and `--state all` are two different questions, and the
  # whole PR-opening guard turns on telling them apart.
  "pr list")
    case "$*" in
      *"--state all"*) cat "$AP_FIXTURES/pr_list_all.json" ;;
      *)               cat "$AP_FIXTURES/pr_list.json" ;;
    esac
    exit 0 ;;
  "pr view")    cat "$AP_FIXTURES/pr_view.json"; exit 0 ;;
  "pr merge")   echo "merged"; exit 0 ;;
  # A body starting with `!` is GitHub refusing, on stderr, non-zero — the
  # shape `create_pr` classifies rather than errors on.
  "pr create")
    body=$(cat "$AP_FIXTURES/pr_create.txt")
    case "$body" in
      "!"*) echo "${body#!}" >&2; exit 1 ;;
      *)    echo "$body"; exit 0 ;;
    esac ;;
  # `gh repo view` takes the repo POSITIONALLY. Asserted here because the
  # real binary rejects `--repo` outright and no fixture would notice.
  "repo view")
    if [ "$3" != "owner/repo" ]; then
      echo "repo view wants the repo positionally, got: $*" >&2; exit 1
    fi
    cat "$AP_FIXTURES/repo_view.json"; exit 0 ;;
  "issue view") echo '{"labels":[{"name":"agent:ready"}]}'; exit 0 ;;
  "issue edit") exit 0 ;;
  "issue comment") exit 0 ;;
esac
if [ "$1" = "api" ]; then
  case "$*" in
    *protection*) echo "gh: Branch not protected (HTTP 404)" >&2; exit 1 ;;
    *)            echo "[[]]"; exit 0 ;;
  esac
fi
exit 0
"#,
    );
    stub(
        &stubs,
        "codex",
        r#"#!/bin/bash
echo "codex $*" >> "$AP_LOG"
out=""; prev=""
for a in "$@"; do
  if [ "$prev" = "-o" ]; then out="$a"; fi
  prev="$a"
done
if [ -n "$out" ]; then
  printf '{"verdict":"pass","risk_class":"documentation","reason":"stubbed reviewer"}' > "$out"
fi
echo '{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5}}}'
exit 0
"#,
    );

    let original_path = std::env::var("PATH").unwrap_or_default();
    std::env::set_var(
        "PATH",
        format!("{}:{original_path}", stubs.to_string_lossy()),
    );
    std::env::set_var("AP_LOG", &log);
    std::env::set_var("AP_FIXTURES", &fixtures);

    // ── the asserted guard ──────────────────────────────────────────────
    // Rung 5's lesson 16, sixth time: a stub that does not actually shadow
    // the real binary turns a free test into a paid one, silently.
    assert_eq!(
        ironmem::autopilot::gh::resolve_gh_binary().unwrap(),
        stubs.join("gh"),
        "gh must resolve to the stub"
    );
    assert_eq!(
        ironmem::autopilot::review::resolve_codex_binary().unwrap(),
        stubs.join("codex"),
        "codex must resolve to the stub"
    );

    // ── a real checkout with a real worktree on the issue's branch ───────
    git(&checkout, &["init", "-q", "--initial-branch=main"]);
    git(&checkout, &["config", "user.email", "t@example.com"]);
    git(&checkout, &["config", "user.name", "T"]);
    std::fs::write(checkout.join("README.md"), "seed\n").unwrap();
    git(&checkout, &["add", "README.md"]);
    git(&checkout, &["commit", "-qm", "seed"]);
    let wt = worktree::ensure_worktree(&checkout, &worktree_root, &issue(), "HEAD").unwrap();
    let head_sha = worktree::resolve_commit(&wt.path, "HEAD").expect("a real commit");

    let write_fixtures = |labels: &str, pr_list: &str, head: &str| {
        std::fs::write(
            fixtures.join("issue_list.json"),
            format!(
                r#"[{{"number":7,"title":"t","body":"b","labels":[{labels}],"updatedAt":"2026-09-03T00:00:00Z"}}]"#
            ),
        )
        .unwrap();
        std::fs::write(fixtures.join("pr_list.json"), pr_list).unwrap();
        // Default: whatever is open is also all there has ever been.
        std::fs::write(fixtures.join("pr_list_all.json"), pr_list).unwrap();
        std::fs::write(
            fixtures.join("repo_view.json"),
            r#"{"defaultBranchRef":{"name":"main"}}"#,
        )
        .unwrap();
        std::fs::write(
            fixtures.join("pr_create.txt"),
            "https://github.com/owner/repo/pull/42",
        )
        .unwrap();
        std::fs::write(
            fixtures.join("pr_view.json"),
            format!(
                r#"{{"number":42,"state":"OPEN","isDraft":false,"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","baseRefName":"main","headRefName":"{}","headRefOid":"{head}","reviewDecision":"APPROVED"}}"#,
                worktree::branch_name(&issue())
            ),
        )
        .unwrap();
    };
    let one_pr = |head: &str| {
        format!(
            r#"[{{"number":42,"headRefName":"{}","headRefOid":"{head}","baseRefName":"main","isDraft":false,"url":"u"}}]"#,
            worktree::branch_name(&issue())
        )
    };
    write_fixtures(
        r#"{"name":"agent:ready"},{"name":"risk:documentation"}"#,
        &one_pr(&head_sha),
        &head_sha,
    );

    let db_path = root.path().join("mem.sqlite3");
    let db = Database::open(&db_path).unwrap();
    db.migrate().unwrap();

    let base_config = |merge: bool, dry_run: bool| AdvanceConfig {
        targets: vec![RepoTarget {
            repo: REPO.to_string(),
            path: checkout.clone(),
            base: "HEAD".to_string(),
        }],
        max_issues_per_repo: 50,
        max_advances_per_pass: DEFAULT_MAX_ADVANCES_PER_PASS,
        merge,
        // Rung 10's paths, exercised exactly as rung 10 shipped them: a
        // `needs_changes` verdict here still holds the PR for a human.
        remediate: false,
        attempt_cap: 5,
        strategy: MergeStrategy::Squash,
        delete_branch: false,
        dry_run,
        daily_budget_usd: 25.0,
        max_unpriced_reviews_per_day: 20,
        worktree_root: worktree_root.clone(),
    };
    let run = |merge: bool, dry_run: bool| {
        let mut gh = GhCli::resolve(&checkout).unwrap();
        let mut reviewer = CodexReviewer::resolve(None).unwrap();
        advance_pass(&db, &mut gh, &mut reviewer, &base_config(merge, dry_run)).unwrap()
    };
    // Every logged `gh` invocation whose line starts with `prefix`.
    let gh_calls = |prefix: &str| -> Vec<String> {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .lines()
            .filter(|line| line.starts_with(prefix))
            .map(str::to_string)
            .collect()
    };
    let codex_calls = || {
        std::fs::read_to_string(&log)
            .unwrap_or_default()
            .matches("codex ")
            .count()
    };

    // ── path 1: an unapproved repo advances nothing, and spends nothing ──
    let report = run(false, false);
    assert_eq!(report.skipped[0].reason, SkipReason::RepoNotApproved);
    assert!(report.advanced.is_empty());
    assert_eq!(codex_calls(), 0, "an unapproved repo must not be reviewed");

    gate_config::propose_gate_config(
        &db,
        REPO,
        vec!["cargo test --workspace".to_string()],
        Vec::new(),
    )
    .unwrap();
    gate_config::approve_gate_config(&db, REPO).unwrap();

    // ── path 2: approved, but nothing has gone green yet ─────────────────
    let report = run(false, false);
    assert_eq!(report.skipped[0].reason, SkipReason::NoSuccessYet);
    assert_eq!(codex_calls(), 0);

    // The green run this rung exists to finish.
    lineage::upsert_issue_status(
        &db,
        &IssueStatus {
            issue: issue(),
            best_verdict: Some(AttemptOutcome::Success),
            best_commit_sha: Some(head_sha.clone()),
            cumulative_attempt_n: 1,
        },
    )
    .unwrap();

    // ── path 3: a dry run reads everything and spends nothing ────────────
    let report = run(false, true);
    assert!(report.dry_run);
    assert!(report.advanced[0].review.is_none());
    assert!(report.advanced[0].merge.is_none());
    assert_eq!(codex_calls(), 0, "a dry run must not pay for a review");

    // ── path 4: no PR has ever existed, so this pass opens one ───────────
    //
    // ⟨defect 7⟩ Rung 10 asserted a stall here, and that assertion was the
    // defect wearing a test: the IC was supposed to have opened the pull
    // request and nothing ever did, so an issue that went green sat at
    // `NoOpenPr` on every pass for ever — unrecoverable, because `run_issue`
    // will not re-dispatch an issue whose lineage records a success.
    std::fs::write(fixtures.join("pr_list.json"), "[]").unwrap();
    std::fs::write(fixtures.join("pr_list_all.json"), "[]").unwrap();
    let report = run(false, false);
    match &report.advanced[0].step {
        AdvanceStep::OpenedPr {
            pr_number,
            base_branch,
            ..
        } => {
            assert_eq!(*pr_number, 42);
            assert_eq!(base_branch, "main", "read from `gh repo view`");
        }
        other => panic!("expected a pull request to be opened, got {other:?}"),
    }
    assert_eq!(codex_calls(), 0, "opening the PR is the whole step");
    let created = gh_calls("gh pr create");
    assert_eq!(created.len(), 1);
    assert!(
        created[0].contains(&format!("--head {}", worktree::branch_name(&issue()))),
        "{}",
        created[0]
    );
    assert!(
        created[0].contains("--base main"),
        "the base is GitHub's default branch, never the target's `HEAD` \
         committish: {}",
        created[0]
    );

    // ── path 4b: a PR that already existed here is never re-opened ────────
    //
    // The guard that makes path 4 safe. A human closing a pull request is a
    // decision; a cron entry re-opening it every few minutes overrules that
    // decision, silently.
    std::fs::write(
        fixtures.join("pr_list_all.json"),
        format!(
            r#"[{{"number":41,"headRefName":"{}","headRefOid":"{head_sha}","baseRefName":"main","isDraft":false,"url":"u"}}]"#,
            worktree::branch_name(&issue())
        ),
    )
    .unwrap();
    let before = gh_calls("gh pr create").len();
    let report = run(false, false);
    match &report.advanced[0].step {
        AdvanceStep::Stalled(Stall::NoOpenPr {
            reason: NoPrReason::PriorPr { numbers },
            ..
        }) => assert_eq!(numbers, &vec![41]),
        other => panic!("expected a prior-PR stall, got {other:?}"),
    }
    assert_eq!(
        gh_calls("gh pr create").len(),
        before,
        "nothing may be created once a pull request has existed here"
    );

    // ── path 4c: a branch that never reached the remote says so ───────────
    std::fs::write(fixtures.join("pr_list_all.json"), "[]").unwrap();
    std::fs::write(
        fixtures.join("pr_create.txt"),
        "!pull request create failed: GraphQL: No commits between main and \
         autopilot/owner-repo-7",
    )
    .unwrap();
    let report = run(false, false);
    assert!(
        matches!(
            &report.advanced[0].step,
            AdvanceStep::Stalled(Stall::NoOpenPr {
                reason: NoPrReason::BranchNotPushed { .. },
                ..
            })
        ),
        "got {:?}",
        report.advanced[0].step
    );
    assert!(
        report.problems.is_empty(),
        "GitHub refusing for a reason we can name is a stall, not a problem: {:?}",
        report.problems
    );
    std::fs::write(
        fixtures.join("pr_create.txt"),
        "https://github.com/owner/repo/pull/42",
    )
    .unwrap();

    // ── path 5: two open PRs fail closed ─────────────────────────────────
    std::fs::write(
        fixtures.join("pr_list.json"),
        format!(
            r#"[{{"number":42,"headRefName":"{b}","headRefOid":"{head_sha}","baseRefName":"main","isDraft":false,"url":"u"}},{{"number":43,"headRefName":"{b}","headRefOid":"{head_sha}","baseRefName":"release","isDraft":false,"url":"u"}}]"#,
            b = worktree::branch_name(&issue())
        ),
    )
    .unwrap();
    let report = run(false, false);
    match &report.advanced[0].step {
        AdvanceStep::Stalled(Stall::AmbiguousPr { numbers }) => assert_eq!(numbers, &vec![42, 43]),
        other => panic!("expected an ambiguity stall, got {other:?}"),
    }
    assert_eq!(codex_calls(), 0, "an ambiguous PR must not be reviewed");

    std::fs::write(fixtures.join("pr_list.json"), one_pr(&head_sha)).unwrap();
    std::fs::write(fixtures.join("pr_list_all.json"), one_pr(&head_sha)).unwrap();

    // ── path 6: the review runs, and the merge is only rehearsed ─────────
    let report = run(false, false);
    assert_eq!(codex_calls(), 1, "the reviewer ran exactly once");
    let advanced = &report.advanced[0];
    assert!(matches!(advanced.step, AdvanceStep::Review { .. }));
    assert_eq!(advanced.dispatch_class, "documentation");
    assert!(matches!(
        advanced.merge.as_ref().unwrap().outcome,
        MergeOutcome::WouldMerge { .. }
    ));
    assert!(advanced.cleanup.is_none(), "a rehearsal cleans nothing");
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(
        !calls.contains("gh pr merge"),
        "no merge was executed: {calls}"
    );
    // The real argv, asserted rather than assumed.
    assert!(calls.contains("codex exec -s read-only"), "{calls}");
    assert!(
        calls.contains("gh pr list --repo owner/repo --head"),
        "{calls}"
    );

    // ── path 7: the same head is not reviewed, or re-billed, twice ───────
    let report = run(false, false);
    assert_eq!(
        codex_calls(),
        1,
        "a head that has not moved is not re-reviewed"
    );
    assert!(matches!(report.advanced[0].step, AdvanceStep::Merge { .. }));

    // ── path 8: --merge lands it, and the loop finally closes ────────────
    // A drawer a paused run left behind, holding a concurrency slot.
    dispatch_state::upsert_dispatch_state(
        &db,
        &DispatchState {
            issue: issue(),
            worktree_path: wt.path.to_string_lossy().to_string(),
            ic_session_name: "autopilot-ic-owner-repo-7".into(),
            dispatch_class: "documentation".into(),
            attempt_n: 1,
            state: "paused-daily-budget".into(),
            started_at: "2026-09-03T00:00:00Z".into(),
            session_uuid: "11111111-2222-3333-4444-555555555555".into(),
            turn_n: 1,
            session_claimed: true,
        },
    )
    .unwrap();

    let report = run(true, false);
    assert!(report.merge_enabled);
    let advanced = &report.advanced[0];
    let outcome = &advanced.merge.as_ref().unwrap().outcome;
    assert!(outcome.landed(), "the PR must land, got {outcome:?}");

    let cleanup = advanced
        .cleanup
        .as_ref()
        .expect("a landed PR is cleaned up");
    assert!(cleanup.error.is_none(), "{:?}", cleanup.error);
    assert!(cleanup.dispatch_state_cleared, "the slot is given back");
    assert!(
        matches!(cleanup.worktree, WorktreeRemoval::Removed { .. }),
        "the worktree is given back, got {:?}",
        cleanup.worktree
    );
    assert!(!wt.path.exists(), "the checkout is gone from disk");
    assert!(dispatch_state::get_dispatch_state(&db, &issue())
        .unwrap()
        .is_none());

    // ── the money question, asked explicitly ─────────────────────────────
    // Codex reports no price, so the ledger's dollars stay at zero and the
    // reviews are counted as unpriced — never as free.
    let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
    let ledger = ironmem::autopilot::budget::get_daily_spend(&db, &today).unwrap();
    if let Some(ledger) = ledger {
        assert_eq!(
            ledger.total_cost_usd, 0.0,
            "the whole run spent $0.00 in reported dollars"
        );
    }

    std::env::set_var("PATH", original_path);
}
