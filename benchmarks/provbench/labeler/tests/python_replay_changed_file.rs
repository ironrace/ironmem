//! Plan A.1 retrospective: short-circuit Python replay for changed files.
//!
//! When a Python source file changes between T₀ and a post-commit, the
//! labeler cannot mechanically classify its facts via the existing
//! Rust-oriented AST match path (`matching_post_fact` was Rust-only,
//! and `RustAst::parse` applied to Python source returns a garbled tree
//! that the disambiguator contract was never written for).
//!
//! Pre-fix: `Replay::run` panicked at `replay/match_post.rs:60` because
//! Plan A Task 12's Python facts have `function_signature_disambiguator
//! = None` while `post_ast` was nevertheless `Some(garbled_RustAst)`.
//!
//! Post-fix contract: Python facts at a changed file route to
//! `Label::NeedsRevalidation` — the existing rule-chain escape hatch
//! meaning "structurally unclassifiable, ask the baseline". Full Python
//! replay (PythonAst post-cache + Python-flavored matching) is out of
//! scope for v1.2b and is documented as a labeler limitation.

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
    assert!(status.success(), "git {args:?} failed in {}", repo.display());
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

#[test]
fn python_fact_at_changed_file_emits_needs_revalidation() {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test"]);

    // Commit 1 (T₀): src/foo.py with `def f(): return 1`.
    let foo_path = repo.join("src/foo.py");
    std::fs::create_dir_all(foo_path.parent().unwrap()).unwrap();
    std::fs::write(&foo_path, "def f():\n    return 1\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "initial"]);
    let t0 = capture(repo, &["rev-parse", "HEAD"]);

    // Commit 2: change the function body (signature byte-identical,
    // body bytes differ → file_byte_identical bypass MUST NOT fire,
    // forcing the per-fact classification path).
    std::fs::write(&foo_path, "def f():\n    return 2\n").unwrap();
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "change body"]);
    let head = capture(repo, &["rev-parse", "HEAD"]);
    assert_ne!(head, t0, "two distinct commits required");

    let cfg = ReplayConfig {
        repo_path: repo.to_path_buf(),
        t0_sha: t0,
        skip_symbol_resolution: true,
    };

    // Pre-fix this panics; post-fix this returns a deterministic row set.
    let rows = Replay::run(&cfg).expect("Replay::run on Python repo must not panic");

    // Locate the FunctionSignature fact for src.foo.f at commit 2.
    let foo_func_row = rows
        .iter()
        .find(|r| {
            r.commit_sha == head
                && r.fact_id.starts_with("FunctionSignature::")
                && r.fact_id.contains("foo.py")
        })
        .unwrap_or_else(|| {
            panic!(
                "no FunctionSignature row for src.foo.f at commit {head}; got {} rows: {:?}",
                rows.len(),
                rows.iter().map(|r| &r.fact_id).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        foo_func_row.label,
        Label::NeedsRevalidation,
        "Python fact at a changed file must route to NeedsRevalidation (Option C \
         short-circuit); got {:?}",
        foo_func_row.label
    );
}
