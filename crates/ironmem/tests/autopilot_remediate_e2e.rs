//! Rung 11 end-to-end: the red path, against real everything.
//!
//! A real SQLite database, a real git checkout with a real worktree on the
//! issue's branch, the real `GhCli`, `CodexReviewer` and `ClaudeDispatcher`
//! runners, and the real argv each of them builds. Only the three binaries are
//! stubbed — behind an **asserted PATH guard**, so a stub that fails to shadow
//! the real thing fails the test rather than quietly spending money.
//!
//! Nothing here can reach the network or an API. **$0.00.**
//!
//! What it proves that the unit tests cannot: the loop *closes*. A reviewer's
//! `needs_changes` arms a re-dispatch, the Lead's own runner picks the issue up
//! **despite its recorded success**, the findings reach the IC through the real
//! `claude` argv, the IC's pushed fix supersedes the remediation, and the next
//! advance pass re-reviews the new head and merges it. Every one of those five
//! steps crosses a module boundary that a unit test stubs out.

use std::path::{Path, PathBuf};

use ironmem::autopilot::advance::{advance_pass, AdvanceConfig, DEFAULT_MAX_ADVANCES_PER_PASS};
use ironmem::autopilot::gh::{GhCli, MergeStrategy};
use ironmem::autopilot::lead::RepoTarget;
use ironmem::autopilot::merge::MergeOutcome;
use ironmem::autopilot::remediate::{self, ArmOutcome};
use ironmem::autopilot::review::CodexReviewer;
use ironmem::autopilot::run::{run_issue, ClaudeDispatcher, IssueBrief, RunConfig, TerminalReason};
use ironmem::autopilot::worktree;
use ironmem::autopilot::{gate_config, IssueRef};
use ironmem::db::schema::Database;

const REPO: &str = "owner/repo";
const ATTEMPT_CAP: u32 = 5;

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

fn stub(dir: &Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// The whole rung in one sequential test.
///
/// One test rather than seven, because it installs stubs on `PATH` for the
/// whole process: two of these running concurrently would each see the other's
/// fixtures. (Rung 10's review round: environment variables are process-global
/// and this suite is threaded, so a test that mutates one is mutating it for
/// every other test in the process.)
#[test]
#[cfg(unix)]
fn the_red_path_closes_end_to_end() {
    let root = tempfile::tempdir().unwrap();
    let stubs = root.path().join("stubs");
    let fixtures = root.path().join("fixtures");
    let checkout = root.path().join("checkout");
    let worktree_root = root.path().join("worktrees");
    let log = root.path().join("calls.log");
    for dir in [&stubs, &fixtures, &checkout, &worktree_root] {
        std::fs::create_dir_all(dir).unwrap();
    }

    // ── stubs ───────────────────────────────────────────────────────────
    stub(
        &stubs,
        "gh",
        r#"#!/bin/bash
echo "gh $*" >> "$AP_LOG"
case "$1 $2" in
  "issue list") cat "$AP_FIXTURES/issue_list.json"; exit 0 ;;
  "pr list")    cat "$AP_FIXTURES/pr_list.json"; exit 0 ;;
  "pr view")    cat "$AP_FIXTURES/pr_view.json"; exit 0 ;;
  "pr merge")   echo "merged"; exit 0 ;;
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

    // The reviewer's verdict comes from a file, so the same real
    // `CodexReviewer` can say NEEDS CHANGES on one pass and PASS on the next.
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
if [ -n "$out" ]; then cat "$AP_FIXTURES/verdict.json" > "$out"; fi
echo '{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5}}}'
exit 0
"#,
    );

    // The IC. It records the condition it was handed — which is how the test
    // asserts the findings reached the *real* argv — and, when told to, makes
    // a real commit, so the branch head genuinely moves.
    stub(
        &stubs,
        "claude",
        r#"#!/bin/bash
# This stub commits, so the same rule applies to it: an inherited GIT_DIR
# would aim `git commit` at a repository nothing in this test chose.
unset $(env | grep -o '^GIT_[A-Za-z0-9_]*' || true)
echo "claude $*" >> "$AP_LOG"
prev=""
for a in "$@"; do
  if [ "$prev" = "-p" ]; then printf '%s' "$a" > "$AP_FIXTURES/last_condition.txt"; fi
  prev="$a"
done
if [ -f "$AP_FIXTURES/ic_should_commit" ]; then
  echo "fix $(date +%s%N)" >> fix.txt
  git add fix.txt >/dev/null 2>&1
  git -c user.email=ic@example.com -c user.name=IC commit -qm "address review findings" >/dev/null 2>&1
fi
printf '{"total_cost_usd":0.0,"num_turns":2,"duration_ms":5,"is_error":false,"session_id":"11111111-2222-3333-4444-555555555555","structured_output":{"verdict":"met"}}'
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
    // Lesson 16, seventh time. A stub that does not actually shadow the real
    // binary turns a free test into a paid one, silently — and `claude` is the
    // one that costs real money per dispatch.
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
    assert_eq!(
        ironmem::autopilot::dispatch::resolve_claude_binary().unwrap(),
        stubs.join("claude"),
        "claude must resolve to the stub — this is the one that spends money"
    );

    // ── a real checkout with a real worktree on the issue's branch ───────
    git(&checkout, &["init", "-q", "--initial-branch=main"]);
    git(&checkout, &["config", "user.email", "t@example.com"]);
    git(&checkout, &["config", "user.name", "T"]);
    std::fs::write(checkout.join("README.md"), "seed\n").unwrap();
    git(&checkout, &["add", "README.md"]);
    git(&checkout, &["commit", "-qm", "seed"]);
    let wt = worktree::ensure_worktree(&checkout, &worktree_root, &issue(), "HEAD").unwrap();

    let db_path = root.path().join("mem.sqlite3");
    let db = Database::open(&db_path).unwrap();
    db.migrate().unwrap();
    gate_config::propose_gate_config(&db, REPO, vec!["true".to_string()], Vec::new()).unwrap();
    gate_config::approve_gate_config(&db, REPO).unwrap();

    let head = || worktree::resolve_commit(&wt.path, "HEAD").expect("a real commit");
    let write_fixtures = |head_sha: &str| {
        std::fs::write(
            fixtures.join("issue_list.json"),
            r#"[{"number":7,"title":"t","body":"b","labels":[{"name":"agent:ready"},{"name":"risk:documentation"}],"updatedAt":"2026-09-03T00:00:00Z"}]"#,
        )
        .unwrap();
        std::fs::write(
            fixtures.join("pr_list.json"),
            format!(
                r#"[{{"number":42,"headRefName":"{}","headRefOid":"{head_sha}","baseRefName":"main","isDraft":false,"url":"u"}}]"#,
                worktree::branch_name(&issue())
            ),
        )
        .unwrap();
        std::fs::write(
            fixtures.join("pr_view.json"),
            format!(
                r#"{{"number":42,"state":"OPEN","isDraft":false,"mergeable":"MERGEABLE","mergeStateStatus":"CLEAN","baseRefName":"main","headRefName":"{}","headRefOid":"{head_sha}","reviewDecision":"APPROVED"}}"#,
                worktree::branch_name(&issue())
            ),
        )
        .unwrap();
    };
    let set_verdict = |verdict: &str, reason: &str| {
        std::fs::write(
            fixtures.join("verdict.json"),
            format!(
                r#"{{"verdict":"{verdict}","risk_class":"documentation","reason":"{reason}"}}"#
            ),
        )
        .unwrap();
    };

    let run_config = || {
        let mut config = RunConfig::new("claude-sonnet-5", "documentation");
        config.attempt_cap = ATTEMPT_CAP;
        config
    };
    let advance_config = |remediate: bool| AdvanceConfig {
        targets: vec![RepoTarget {
            repo: REPO.to_string(),
            path: checkout.clone(),
            base: "HEAD".to_string(),
        }],
        max_issues_per_repo: 50,
        max_advances_per_pass: DEFAULT_MAX_ADVANCES_PER_PASS,
        merge: true,
        remediate,
        attempt_cap: ATTEMPT_CAP,
        strategy: MergeStrategy::Squash,
        delete_branch: false,
        dry_run: false,
        daily_budget_usd: 25.0,
        max_unpriced_reviews_per_day: 20,
        worktree_root: worktree_root.clone(),
    };
    let advance = |remediate: bool| {
        let mut gh = GhCli::resolve(&checkout).unwrap();
        let mut reviewer = CodexReviewer::resolve(None).unwrap();
        advance_pass(&db, &mut gh, &mut reviewer, &advance_config(remediate)).unwrap()
    };
    let dispatch_ic = || {
        let mut dispatcher = ClaudeDispatcher::resolve().unwrap();
        run_issue(
            &db,
            &issue(),
            &IssueBrief {
                title: "t".into(),
                body: "b".into(),
            },
            &wt,
            &run_config(),
            &mut dispatcher,
        )
        .unwrap()
    };

    // ── 1. the IC goes green, exactly as rungs 2/4 leave it ─────────────
    let first = dispatch_ic();
    assert!(
        matches!(first.terminal, TerminalReason::Met { .. }),
        "got {:?}",
        first.terminal
    );
    let green_commit = head();
    write_fixtures(&green_commit);

    // ── 2. a reviewer asks for changes; the re-dispatch is armed ────────
    set_verdict("needs_changes", "the retry loop is unbounded");
    let report = advance(true);
    let advanced = &report.advanced[0];
    assert!(
        matches!(advanced.remediation, Some(ArmOutcome::Armed { .. })),
        "got {:?}",
        advanced.remediation
    );
    let execution = advanced.merge.as_ref().unwrap();
    assert!(
        matches!(execution.outcome, MergeOutcome::Held(_)),
        "never merged with an unresolved finding, got {:?}",
        execution.outcome
    );
    assert!(
        execution.label_plan.is_none() && !execution.commented,
        "the merge is rehearsed while a remediation is in force, so `agent:ready` \
survives and the Lead can still see the issue"
    );

    // ── 3. the Lead re-dispatches a *succeeded* issue, and the IC is told ─
    //
    // The step rung 10 could not take. Without the remediation this run
    // returns `AlreadySucceeded` and dispatches nothing.
    std::fs::write(fixtures.join("ic_should_commit"), "yes").unwrap();
    let second = dispatch_ic();
    assert!(
        matches!(second.terminal, TerminalReason::Met { .. }),
        "the remediation dispatch must actually run, got {:?}",
        second.terminal
    );

    let condition = std::fs::read_to_string(fixtures.join("last_condition.txt")).unwrap();
    assert!(
        condition.contains("the retry loop is unbounded"),
        "the reviewer's findings must reach the real argv: {condition}"
    );
    assert!(
        condition.contains("pull request #42"),
        "the IC is told which PR it is fixing"
    );
    assert!(
        condition.contains("addressed by a commit you have pushed to this branch"),
        "the goal condition must extend past the already-green gate, or the IC \
satisfies it by doing nothing"
    );

    // ── 4. the pushed fix supersedes the remediation ────────────────────
    let fixed_commit = head();
    assert_ne!(fixed_commit, green_commit, "the IC actually committed");
    assert_eq!(
        remediate::active_remediation(&db, &issue()).unwrap(),
        None,
        "a newer success ends the remediation — no flag, no write, just the \
commit having moved"
    );
    assert!(
        remediate::get_remediation(&db, &issue()).unwrap().is_some(),
        "the record itself is kept as the audit trail"
    );

    // ── 5. the new head is re-reviewed, passes, and merges ──────────────
    write_fixtures(&fixed_commit);
    set_verdict("pass", "the loop is bounded now");
    let report = advance(true);
    let advanced = &report.advanced[0];
    assert_eq!(
        advanced.remediation, None,
        "a passing review arms nothing, got {:?}",
        advanced.remediation
    );
    let execution = advanced.merge.as_ref().unwrap();
    assert!(
        execution.outcome.landed(),
        "the fix is reviewed and merged: the loop closes, got {:?}",
        execution.outcome
    );
    assert!(
        advanced.cleanup.is_some(),
        "a landed PR gives its worktree and slot back"
    );

    // ── nothing was ever paid for ───────────────────────────────────────
    let calls = std::fs::read_to_string(&log).unwrap();
    assert!(calls.contains("codex exec"), "the real reviewer argv ran");
    assert!(
        calls.contains("--dangerously-skip-permissions"),
        "the real IC argv ran"
    );
    std::env::set_var("PATH", original_path);
}
