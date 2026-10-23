//! Public-CLI coverage for selecting the Codex implementation harness.
//!
//! The assistant binary is a local stub on an isolated PATH. Nothing in this
//! test can reach a model or GitHub.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ironmem::autopilot::gate_config;
use ironmem::db::schema::Database;

const REPO: &str = "owner/repo";

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

fn git(dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    let output = command.args(args).current_dir(dir).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn make_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "--initial-branch=main"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    std::fs::write(path.join("README.md"), "seed\n").unwrap();
    git(path, &["add", "README.md"]);
    git(path, &["commit", "-qm", "seed"]);
}

fn approve(db_path: &Path) {
    let db = Database::open(db_path).unwrap();
    db.migrate().unwrap();
    gate_config::propose_gate_config(&db, REPO, vec!["true".into()], Vec::new()).unwrap();
    gate_config::approve_gate_config(&db, REPO).unwrap();
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn isolated_path(stubs: &Path) -> PathBuf {
    let mut paths = vec![stubs.to_path_buf()];
    paths.extend(
        ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .into_iter()
            .map(PathBuf::from),
    );
    std::env::join_paths(paths).unwrap().into()
}

#[test]
fn public_run_selects_codex_without_resolving_claude() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkout");
    let worktrees = root.path().join("worktrees");
    let stubs = root.path().join("stubs");
    let db_path = root.path().join("memory.sqlite3");
    let calls = root.path().join("codex.calls");
    let count = root.path().join("codex.count");
    make_repo(&checkout);
    std::fs::create_dir_all(&stubs).unwrap();
    approve(&db_path);

    executable(
        &stubs.join("codex"),
        r#"#!/bin/sh
printf '%s\n' "$PWD|$*" >> "$CODEX_CALLS"
args="$*"
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; final="$1"; fi
  shift
done
printf '%s\n' '{"type":"thread.started","thread_id":"11111111-2222-3333-4444-555555555555"}'
printf '%s\n' '{"type":"turn.completed","usage":{}}'
if [ ! -f "$CODEX_COUNT" ]; then
  printf '1\n' > "$CODEX_COUNT"
  printf '%s\n' '{"verdict":"not_met","reason":"keep working"}' > "$final"
else
  case "$args" in
    *"exec resume"*"11111111-2222-3333-4444-555555555555"*) ;;
    *) printf 'resume argv did not preserve native identity: %s\n' "$args" >&2; exit 42 ;;
  esac
  printf '42\n' > answer.txt
  printf '%s\n' '{"verdict":"met","reason":"schema-valid completion"}' > "$final"
fi
"#,
    );

    let output = Command::new(bin())
        .args([
            "autopilot",
            "run",
            REPO,
            "7",
            "--title",
            "exercise Codex selection",
            "--implementer",
            "codex",
            "--model",
            "gpt-test",
            "--attempt-cap",
            "2",
            "--db",
            db_path.to_str().unwrap(),
            "--path",
            checkout.to_str().unwrap(),
            "--worktree-root",
            worktrees.to_str().unwrap(),
            "--json",
        ])
        .env("PATH", isolated_path(&stubs))
        .env("CODEX_CALLS", &calls)
        .env("CODEX_COUNT", &count)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "run failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        report["unpriced_dispatches"],
        2,
        "unexpected report: {report}; calls: {}",
        std::fs::read_to_string(&calls).unwrap_or_default()
    );
    assert_eq!(report["total_cost_usd"], 0.0);
    assert_eq!(report["dispatches"].as_array().unwrap().len(), 2);
    assert_eq!(
        report["dispatches"][0]["total_cost_usd"],
        serde_json::Value::Null
    );
    assert_eq!(
        report["dispatches"][1]["total_cost_usd"],
        serde_json::Value::Null
    );
    assert!(
        report["terminal"]["terminal"] == "met",
        "strict Met verdict must be the terminal result: {report}"
    );
    let calls = std::fs::read_to_string(&calls).unwrap();
    assert_eq!(
        calls.matches("|exec ").count(),
        2,
        "unexpected calls: {calls}"
    );
    assert!(
        calls.contains("|exec --json"),
        "unexpected new argv: {calls}"
    );
    assert!(
        calls.contains("|exec resume --json"),
        "unexpected resume argv: {calls}"
    );
    assert_eq!(
        calls
            .matches(&format!(
                "{}|exec ",
                worktrees.join("owner-repo-7").display()
            ))
            .count(),
        2,
        "Codex must run in the isolated issue worktree: {calls}"
    );
    assert!(
        !calls.contains("--max-turns"),
        "unexpected Claude flag: {calls}"
    );
    assert!(
        !calls.contains("--max-budget-usd"),
        "unexpected Claude flag: {calls}"
    );
    assert!(calls.contains("true, and every commit you have made is pushed"));
}

#[test]
fn public_lead_with_codex_and_advisor_off_does_not_require_claude() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkout");
    let worktrees = root.path().join("worktrees");
    let stubs = root.path().join("stubs");
    let db_path = root.path().join("memory.sqlite3");
    make_repo(&checkout);
    std::fs::create_dir_all(&stubs).unwrap();
    executable(&stubs.join("codex"), "#!/bin/sh\nexit 99\n");
    executable(&stubs.join("gh"), "#!/bin/sh\nprintf '%s\\n' '[]'\n");

    let repo_arg = format!("{REPO}={}", checkout.display());
    let output = Command::new(bin())
        .args([
            "autopilot",
            "lead",
            "--repo",
            &repo_arg,
            "--implementer",
            "codex",
            "--model",
            "gpt-test",
            "--worktree-root",
            worktrees.to_str().unwrap(),
            "--db",
            db_path.to_str().unwrap(),
            "--dry-run",
            "--json",
        ])
        .env("PATH", isolated_path(&stubs))
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "lead failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["dry_run"], true);
}

#[test]
fn codex_native_limit_validation_precedes_model_and_all_persistent_side_effects() {
    let root = tempfile::tempdir().unwrap();
    let empty_path = root.path().join("empty-path");
    std::fs::create_dir_all(&empty_path).unwrap();

    let cases = [
        (
            vec![
                "autopilot".to_string(),
                "run".into(),
                REPO.into(),
                "7".into(),
                "--title".into(),
                "t".into(),
                "--implementer".into(),
                "codex".into(),
                "--max-budget-usd".into(),
                "1".into(),
            ],
            "--max-budget-usd",
            "run.sqlite3",
            "run-worktrees",
        ),
        (
            vec![
                "autopilot".to_string(),
                "lead".into(),
                "--repo".into(),
                "owner/repo=/does/not/exist".into(),
                "--implementer".into(),
                "codex".into(),
                "--max-turns".into(),
                "12".into(),
            ],
            "--max-turns",
            "lead.sqlite3",
            "lead-worktrees",
        ),
    ];

    for (mut args, expected, db_name, worktree_name) in cases {
        let db_path = root.path().join(db_name);
        let worktrees = root.path().join(worktree_name);
        args.extend([
            "--db".into(),
            db_path.display().to_string(),
            "--worktree-root".into(),
            worktrees.display().to_string(),
        ]);
        let output = Command::new(bin())
            .args(&args)
            .env("PATH", &empty_path)
            .output()
            .unwrap();
        assert!(!output.status.success(), "invalid invocation must fail");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(expected), "got: {stderr}");
        assert!(
            !stderr.contains("--model is required"),
            "unsupported explicit limit must win validation order: {stderr}"
        );
        assert!(
            !db_path.exists(),
            "validation must precede database creation"
        );
        assert!(
            !worktrees.exists(),
            "validation must precede worktree provisioning"
        );
    }
}

#[test]
fn codex_requires_an_explicit_model_before_database_creation() {
    let root = tempfile::tempdir().unwrap();
    let db_path = root.path().join("memory.sqlite3");
    let output = Command::new(bin())
        .args([
            "autopilot",
            "run",
            REPO,
            "7",
            "--title",
            "t",
            "--implementer",
            "codex",
            "--db",
            db_path.to_str().unwrap(),
        ])
        .env("PATH", root.path())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--model is required"), "got: {stderr}");
    assert!(
        !db_path.exists(),
        "validation must precede database creation"
    );
}

#[test]
fn public_codex_failures_are_never_success_and_each_consumes_an_unpriced_slot() {
    for (mode, issue) in [("malformed", "21"), ("nonzero", "22"), ("timeout", "23")] {
        let root = tempfile::tempdir().unwrap();
        let checkout = root.path().join("checkout");
        let worktrees = root.path().join("worktrees");
        let stubs = root.path().join("stubs");
        let db_path = root.path().join("memory.sqlite3");
        let child_pid = root.path().join("child.pid");
        make_repo(&checkout);
        std::fs::create_dir_all(&stubs).unwrap();
        approve(&db_path);
        if mode == "timeout" {
            let db = Database::open(&db_path).unwrap();
            gate_config::set_wall_clock_timeout(&db, REPO, Some(1)).unwrap();
        }

        executable(
            &stubs.join("codex"),
            r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; final="$1"; fi
  shift
done
printf '%s\n' '{"type":"thread.started","thread_id":"22222222-3333-4444-8555-666666666666"}'
if [ "$CODEX_MODE" = timeout ]; then
  /bin/sleep 30 &
  child=$!
  printf '%s\n' "$child" > "$CODEX_CHILD_PID"
  wait "$child"
  exit 0
fi
/bin/sleep 0.15
printf '%s\n' '{"type":"turn.completed","usage":{}}'
if [ "$CODEX_MODE" = malformed ]; then
  printf '%s\n' 'not-json' > "$final"
  exit 0
fi
printf '%s\n' '{"verdict":"met","reason":"must be rejected with the exit"}' > "$final"
exit 7
"#,
        );

        let output = Command::new(bin())
            .args([
                "autopilot",
                "run",
                REPO,
                issue,
                "--title",
                "exercise failure classification",
                "--implementer",
                "codex",
                "--model",
                "gpt-test",
                "--attempt-cap",
                "1",
                "--max-unpriced-dispatches-per-day",
                "1",
                "--db",
                db_path.to_str().unwrap(),
                "--path",
                checkout.to_str().unwrap(),
                "--worktree-root",
                worktrees.to_str().unwrap(),
                "--json",
            ])
            .env("PATH", isolated_path(&stubs))
            .env("CODEX_MODE", mode)
            .env("CODEX_CHILD_PID", &child_pid)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "{mode} run failed:\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["unpriced_dispatches"], 1, "{mode}: {report}");
        assert_eq!(report["total_cost_usd"], 0.0, "{mode}: {report}");
        assert_eq!(
            report["dispatches"][0]["total_cost_usd"],
            serde_json::Value::Null,
            "{mode}: {report}"
        );
        assert_ne!(
            report["dispatches"][0]["classification"], "met",
            "{mode}: schema output alone must not establish success: {report}"
        );
        assert_ne!(report["terminal"]["terminal"], "met", "{mode}: {report}");

        if mode == "timeout" {
            let pid = std::fs::read_to_string(&child_pid).unwrap();
            let alive = Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .output()
                .unwrap()
                .status
                .success();
            assert!(!alive, "timed-out Codex child {pid:?} must be reaped");
        }
    }
}

#[test]
fn selected_binary_is_resolved_after_approval_but_before_worktree_provisioning() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkout");
    let worktrees = root.path().join("worktrees");
    let empty_path = root.path().join("empty-path");
    let db_path = root.path().join("memory.sqlite3");
    make_repo(&checkout);
    std::fs::create_dir_all(&empty_path).unwrap();
    approve(&db_path);

    let output = Command::new(bin())
        .args([
            "autopilot",
            "run",
            REPO,
            "30",
            "--title",
            "missing selected binary",
            "--implementer",
            "codex",
            "--model",
            "gpt-test",
            "--db",
            db_path.to_str().unwrap(),
            "--path",
            checkout.to_str().unwrap(),
            "--worktree-root",
            worktrees.to_str().unwrap(),
        ])
        .env("PATH", &empty_path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("`codex`"),
        "selected binary must be named: {stderr}"
    );
    assert!(
        !stderr.contains("claude"),
        "wrong harness was resolved: {stderr}"
    );
    assert!(
        !worktrees.exists(),
        "missing selected binary must not leave a provisioned worktree"
    );

    let unapproved_db = root.path().join("unapproved.sqlite3");
    Database::open(&unapproved_db).unwrap().migrate().unwrap();
    let output = Command::new(bin())
        .args([
            "autopilot",
            "run",
            "owner/unapproved",
            "31",
            "--title",
            "approval diagnostic",
            "--implementer",
            "codex",
            "--model",
            "gpt-test",
            "--db",
            unapproved_db.to_str().unwrap(),
            "--path",
            checkout.to_str().unwrap(),
            "--worktree-root",
            worktrees.to_str().unwrap(),
        ])
        .env("PATH", &empty_path)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("gate config"), "got: {stderr}");
    assert!(!stderr.contains("could not find `codex`"), "got: {stderr}");
    assert!(!worktrees.exists());
}

#[test]
fn human_run_output_labels_known_spend_and_unpriced_dispatches() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkout");
    let worktrees = root.path().join("worktrees");
    let stubs = root.path().join("stubs");
    let db_path = root.path().join("memory.sqlite3");
    make_repo(&checkout);
    std::fs::create_dir_all(&stubs).unwrap();
    approve(&db_path);
    executable(
        &stubs.join("codex"),
        r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; final="$1"; fi
  shift
done
printf '%s\n' '{"type":"thread.started","thread_id":"33333333-4444-4555-8666-777777777777"}'
printf '%s\n' '{"type":"turn.completed","usage":{}}'
printf '%s\n' '{"verdict":"not_met","reason":"not done"}' > "$final"
"#,
    );
    let output = Command::new(bin())
        .args([
            "autopilot",
            "run",
            REPO,
            "40",
            "--title",
            "render accounting",
            "--implementer",
            "codex",
            "--model",
            "gpt-test",
            "--attempt-cap",
            "1",
            "--db",
            db_path.to_str().unwrap(),
            "--path",
            checkout.to_str().unwrap(),
            "--worktree-root",
            worktrees.to_str().unwrap(),
        ])
        .env("PATH", isolated_path(&stubs))
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("known spend this run: $0.0000"), "{stdout}");
    assert!(stdout.contains("1 unpriced dispatch"), "{stdout}");
}

#[test]
fn human_lead_output_labels_known_spend_and_unpriced_dispatches() {
    let root = tempfile::tempdir().unwrap();
    let checkout = root.path().join("checkout");
    let worktrees = root.path().join("worktrees");
    let stubs = root.path().join("stubs");
    let db_path = root.path().join("memory.sqlite3");
    make_repo(&checkout);
    std::fs::create_dir_all(&stubs).unwrap();
    approve(&db_path);
    executable(
        &stubs.join("gh"),
        r#"#!/bin/sh
case "$*" in
  *"--label agent:ready"*)
    printf '%s\n' '[{"number":41,"title":"render lead accounting","body":"body","labels":[{"name":"agent:ready"}],"updatedAt":"2026-10-23T00:00:00Z"}]'
    ;;
  *) printf '%s\n' '[]' ;;
esac
exit 0
"#,
    );
    executable(
        &stubs.join("codex"),
        r#"#!/bin/sh
while [ "$#" -gt 0 ]; do
  if [ "$1" = "-o" ]; then shift; final="$1"; fi
  shift
done
printf '%s\n' '{"type":"thread.started","thread_id":"44444444-5555-4666-8777-888888888888"}'
printf '%s\n' '{"type":"turn.completed","usage":{}}'
printf '%s\n' '{"verdict":"not_met","reason":"not done"}' > "$final"
"#,
    );

    let repo_arg = format!("{REPO}={}", checkout.display());
    let output = Command::new(bin())
        .args([
            "autopilot",
            "lead",
            "--repo",
            &repo_arg,
            "--implementer",
            "codex",
            "--model",
            "gpt-test",
            "--attempt-cap",
            "1",
            "--db",
            db_path.to_str().unwrap(),
            "--worktree-root",
            worktrees.to_str().unwrap(),
        ])
        .env("PATH", isolated_path(&stubs))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "lead failed:\nstdout={}\nstderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("known spend $0.0000"), "{stdout}");
    assert!(stdout.contains("1 unpriced dispatch"), "{stdout}");
}
