//! Python replay tests for changed-file classification.
//!
//! **Plan A.1 retrospective (pre-Task 11):** when a Python source file changed
//! between T₀ and a post-commit, the labeler short-circuited directly to
//! `Label::NeedsRevalidation` for every fact at that path. Full Python AST
//! matching was deferred.
//!
//! **Plan A.2 activation (Task 11):** the short-circuit is removed. Python
//! facts at changed files now flow through `classify_python_against_commit`
//! (dispatched by `classify_against_commit` via `PostAst::Python`).
//!
//! `python_changed_file_no_longer_all_needs_revalidation` is the regression
//! test that verifies the short-circuit is gone: at least one Python fact in a
//! changed file must receive a label other than `NeedsRevalidation`.
//!
//! `python_fact_at_changed_file_emits_needs_revalidation` is **intentionally
//! deleted** as part of Task 11 — it pinned the short-circuit behaviour that
//! Task 11 removes. Task 12 replaces it with per-label assertions against real
//! classification output.

use provbench_labeler::label::Label;
use provbench_labeler::replay::{Replay, ReplayConfig};
use std::path::Path;
use std::process::Command;
use tempfile::TempDir;

fn git(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(repo)
        .args(args)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "git {args:?} failed in {}",
        repo.display()
    );
}

fn capture(repo: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(repo)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?} capture failed");
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// Regression test for SPEC §11 row 2026-05-19 (Plan A.2).
///
/// Builds a synthetic Python repo: T₀ has `def foo(): return 1`; HEAD
/// adds a docstring (function body content hash changes). Pre-Plan-A.2
/// (short-circuit active) the `foo` fact was always `NeedsRevalidation`.
/// Post-Plan-A.2 it should receive a real classification label — anything
/// but `NeedsRevalidation` proves the short-circuit is gone.
#[test]
fn python_changed_file_no_longer_all_needs_revalidation() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test"]);

    // Commit 1 (T₀): src/a.py with `def foo(): return 1`.
    let a_py = repo.join("src/a.py");
    std::fs::create_dir_all(a_py.parent().unwrap()).unwrap();
    std::fs::write(&a_py, "def foo():\n    return 1\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "initial"]);
    let t0 = capture(repo, &["rev-parse", "HEAD"]);

    // Commit 2 (HEAD): add a docstring — body bytes change, signature
    // (`def foo():`) is byte-identical. The file_byte_identical bypass
    // must NOT fire, so facts go through per-fact classification.
    std::fs::write(
        &a_py,
        "def foo():\n    \"\"\"docstring.\"\"\"\n    return 1\n",
    )
    .unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "add docstring"]);
    let head = capture(repo, &["rev-parse", "HEAD"]);
    assert_ne!(head, t0, "two distinct commits required");

    let cfg = ReplayConfig {
        repo_path: repo.to_path_buf(),
        t0_sha: t0,
        skip_symbol_resolution: true,
    };

    let rows = Replay::run(&cfg).expect("Replay::run on Python repo must not panic");

    // There must be at least one fact row for src/a.py at the HEAD commit.
    let foo_facts: Vec<_> = rows
        .iter()
        .filter(|r| r.commit_sha == head && r.fact_id.contains("a.py"))
        .collect();
    assert!(
        !foo_facts.is_empty(),
        "expected at least one fact row for src/a.py at commit {head}; got {} total rows: {:?}",
        rows.len(),
        rows.iter().map(|r| &r.fact_id).collect::<Vec<_>>()
    );

    // At least one fact must NOT be NeedsRevalidation.
    // If every fact is still NeedsRevalidation, the short-circuit was not removed.
    assert!(
        foo_facts
            .iter()
            .any(|f| !matches!(f.label, Label::NeedsRevalidation)),
        "expected at least one fact for src/a.py to NOT be NeedsRevalidation \
         (short-circuit not removed?). Labels: {:?}",
        foo_facts.iter().map(|f| &f.label).collect::<Vec<_>>()
    );
}
