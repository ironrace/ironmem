//! 7-scenario Python integration tests for Plan A.2 label outcomes.
//!
//! Each test builds a synthetic Python git repo (tmpdir + git init + 2 commits),
//! runs Replay against T₀, and asserts the expected Label on the target fact.
//!
//! Helper choice: Option A — a single multi-file `build_synthetic_python_repo`
//! that accepts `(t0_files, head_files)` as slices of `(path, content)`.
//! This generalises cleanly across all 7 scenarios (including the cross-file
//! scenarios 4 and 5), and the single-file scenarios simply pass a one-element
//! slice for each state.
//!
//! ## Implementation vs. plan discrepancies (DONE_WITH_CONCERNS surface)
//!
//! The original plan assumed `skip_symbol_resolution=true` for all scenarios.
//! Three scenarios require the real `CommitSymbolIndex` and rename pipeline:
//!
//! - **Scenario 3** (StaleSymbolRenamed): `skip_symbol_resolution=true` short-
//!   circuits rename detection, returning `StaleSourceDeleted` instead. Tests
//!   use `skip_symbol_resolution=false` so the rename pipeline fires.
//!   Rename pair chosen so leaf-name gates pass: `foo_v1` → `foo_v2` triggers
//!   the version-suffix bypass on MAX_NAME_SIMILARITY.
//!
//! - **Scenarios 4 & 5** (NeedsRevalidation for cross-file): with
//!   `skip_symbol_resolution=true` the labeler cannot see that the symbol moved
//!   to another file and returns `StaleSourceDeleted`. Tests use
//!   `skip_symbol_resolution=false` so `CommitSymbolIndex::lookup_python` sees
//!   the symbol at the new path and routes `NeedsRevalidation`.
//!
//! - **Scenario 1** (StaleSourceChanged): `FunctionSignature` and `PublicSymbol`
//!   span only the `def foo():` header, so body-only changes leave the hash
//!   unchanged → `Valid`. The test uses a `TestAssertion` fact instead, where
//!   the assertion body IS the span.
//!
//! - **Scenario 6** (Valid): git refuses an empty commit when both states are
//!   byte-identical. The test adds a second file that changes at HEAD; the
//!   target file remains unchanged → file_byte_identical bypass → `Valid`.
//!
//! SPEC §11 row 2026-05-19 (Plan A.2).

use provbench_labeler::label::Label;
use provbench_labeler::replay::{FactAtCommit, Replay, ReplayConfig};
use std::path::{Path, PathBuf};
use std::process::Command;
use tempfile::TempDir;

// ── Helpers ──────────────────────────────────────────────────────────────────

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

/// Build a synthetic Python git repo with two commits.
///
/// * `t0_files`   — slice of `(relative_path, content)` written at commit 1 (T₀)
/// * `head_files` — slice of `(relative_path, content)` written at commit 2 (HEAD)
///
/// Returns `(TempDir, t0_sha, head_sha)`. The caller must keep the `TempDir`
/// alive for the duration of the test (dropping it deletes the repo).
fn build_synthetic_python_repo(
    t0_files: &[(&str, &str)],
    head_files: &[(&str, &str)],
) -> (TempDir, String, String) {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path();

    git(repo, &["init", "-q", "-b", "main"]);
    git(repo, &["config", "user.email", "test@example.com"]);
    git(repo, &["config", "user.name", "Test"]);

    // Commit 1 (T₀)
    for (rel_path, content) in t0_files {
        let abs = repo.join(rel_path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, content).unwrap();
    }
    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "initial"]);
    let t0 = capture(repo, &["rev-parse", "HEAD"]);

    // Commit 2 (HEAD): write head_files, then remove any path that existed at
    // T₀ but is absent from head_files (hard deletion).
    let t0_paths: Vec<PathBuf> = t0_files.iter().map(|(p, _)| PathBuf::from(p)).collect();
    let head_paths: Vec<PathBuf> = head_files.iter().map(|(p, _)| PathBuf::from(p)).collect();

    // Write / overwrite HEAD content.
    for (rel_path, content) in head_files {
        let abs = repo.join(rel_path);
        std::fs::create_dir_all(abs.parent().unwrap()).unwrap();
        std::fs::write(&abs, content).unwrap();
    }

    // Delete files that existed at T₀ but are absent at HEAD.
    for t0_rel in &t0_paths {
        if !head_paths.contains(t0_rel) {
            let abs = repo.join(t0_rel);
            if abs.exists() {
                std::fs::remove_file(&abs).unwrap();
            }
        }
    }

    git(repo, &["add", "-A"]);
    git(repo, &["commit", "-q", "-m", "mutate"]);
    let head = capture(repo, &["rev-parse", "HEAD"]);
    assert_ne!(head, t0, "two distinct commits required");

    (tmp, t0, head)
}

/// Run Replay against the synthetic repo and return all facts at HEAD.
///
/// `skip_symbol_resolution` controls whether the `CommitSymbolIndex` is
/// built. Pass `false` for scenarios that require rename detection or
/// cross-file symbol lookup; pass `true` for unit-test scenarios that
/// only need intra-file classification.
fn run_replay(tmp: &TempDir, t0_sha: &str, skip_symbol_resolution: bool) -> Vec<FactAtCommit> {
    let cfg = ReplayConfig {
        repo_path: tmp.path().to_path_buf(),
        t0_sha: t0_sha.to_string(),
        skip_symbol_resolution,
    };
    let rows = Replay::run(&cfg).expect("Replay::run on Python repo must not panic");
    // Return only HEAD rows (the second commit), which carry the final labels.
    let head = capture(tmp.path(), &["rev-parse", "HEAD"]);
    rows.into_iter().filter(|r| r.commit_sha == head).collect()
}

// ── 7 Scenarios ──────────────────────────────────────────────────────────────

/// Scenario 1: assertion body changes → StaleSourceChanged.
///
/// `FunctionSignature` and `PublicSymbol` span only the `def` header, so a
/// body-only change leaves those hashes identical → `Valid`. `TestAssertion`
/// spans the assert statement itself, so changing the assertion expression
/// changes the content hash → `StaleSourceChanged`.
///
/// Filter: only `TestAssertion` facts are examined.
#[test]
fn body_change_routes_to_stale_source_changed() {
    let (tmp, t0_sha, _) = build_synthetic_python_repo(
        &[("tests/test_a.py", "def test_foo():\n    assert 1 == 1\n")],
        &[("tests/test_a.py", "def test_foo():\n    assert 1 == 999\n")],
    );
    let facts = run_replay(&tmp, &t0_sha, true);
    // Locate the TestAssertion fact for test_foo.
    let assertion = facts
        .iter()
        .find(|f| f.fact_id.starts_with("TestAssertion::") && f.fact_id.contains("test_foo"))
        .expect("TestAssertion fact for test_foo");
    assert!(
        matches!(assertion.label, Label::StaleSourceChanged),
        "expected StaleSourceChanged for assertion body change; got {:?}",
        assertion.label
    );
}

/// Scenario 2: symbol deleted from file → StaleSourceDeleted.
#[test]
fn symbol_deletion_routes_to_stale_source_deleted() {
    let (tmp, t0_sha, _) = build_synthetic_python_repo(
        &[("src/a.py", "def foo():\n    return 1\n")],
        &[("src/a.py", "# foo was here\n")],
    );
    let facts = run_replay(&tmp, &t0_sha, true);
    let foo = facts
        .iter()
        .find(|f| f.fact_id.contains(".foo"))
        .expect("foo fact");
    assert!(
        matches!(foo.label, Label::StaleSourceDeleted),
        "expected StaleSourceDeleted for symbol deletion; got {:?}",
        foo.label
    );
}

/// Scenario 3: in-file version-suffix rename → StaleSymbolRenamed.
///
/// `foo_v1` → `foo_v2` with `skip_symbol_resolution=false` so the rename
/// pipeline fires. The version-suffix bypass on MAX_NAME_SIMILARITY lets
/// the high leaf-name similarity pass Gate 4.
///
/// Gate pass conditions:
/// - Container compatibility (Gate 1): both module-level → container = None ✓
/// - T₀ presence exclusion (Gate 2): `foo_v2` absent from T₀ ✓
/// - Span similarity (Gate 3): `def foo_v1():` vs `def foo_v2():` ≈ 0.95 ✓
/// - Leaf similarity with version bypass (Gate 4): ratio ≈ 0.90 ≥ 0.6,
///   version-suffix bypass waives upper bound ✓
#[test]
fn in_file_rename_routes_to_stale_symbol_renamed() {
    let (tmp, t0_sha, _) = build_synthetic_python_repo(
        &[("src/a.py", "def foo_v1():\n    return 1\n")],
        &[("src/a.py", "def foo_v2():\n    return 1\n")],
    );
    let facts = run_replay(&tmp, &t0_sha, false);
    let foo = facts
        .iter()
        .find(|f| f.fact_id.contains(".foo_v1"))
        .expect("foo_v1 fact");
    assert!(
        matches!(foo.label, Label::StaleSymbolRenamed { .. }),
        "expected StaleSymbolRenamed for in-file version-suffix rename; got {:?}",
        foo.label
    );
}

/// Scenario 4: cross-file move of a unique leaf → NeedsRevalidation.
///
/// `unique_leaf` existed only in `src/a.py` at T₀. At HEAD it is removed
/// from `src/a.py` and added to `src/b.py`. With `skip_symbol_resolution=false`
/// the `CommitSymbolIndex` sees the symbol still present in the tree (at a
/// different path) and routes to `NeedsRevalidation` via
/// `PythonLookup::UniqueFallbackAtPath`.
#[test]
fn cross_file_move_unique_leaf_routes_to_needs_revalidation() {
    let (tmp, t0_sha, _) = build_synthetic_python_repo(
        &[("src/a.py", "def unique_leaf():\n    return 1\n")],
        &[
            ("src/a.py", "# moved\n"),
            ("src/b.py", "def unique_leaf():\n    return 1\n"),
        ],
    );
    let facts = run_replay(&tmp, &t0_sha, false);
    let foo = facts
        .iter()
        .find(|f| f.fact_id.contains("unique_leaf"))
        .expect("unique_leaf fact");
    assert!(
        matches!(foo.label, Label::NeedsRevalidation),
        "expected NeedsRevalidation for cross-file move; got {:?}",
        foo.label
    );
}

/// Scenario 5: cross-file module-level leaf name collision → NeedsRevalidation.
///
/// At T₀, `shared_leaf` exists in both `src/a.py` and `src/b.py`.
/// At HEAD, `src/a.py`'s copy is removed; `src/b.py` is unchanged; a third
/// copy appears in `src/c.py`. The T₀ fact anchored at `src.a.shared_leaf`
/// loses its source and the `CommitSymbolIndex` sees the symbol at ≥2 paths
/// → `PythonLookup::AmbiguousFallback` → `NeedsRevalidation`.
#[test]
fn cross_file_collision_module_level_leaf_routes_to_needs_revalidation() {
    let (tmp, t0_sha, _) = build_synthetic_python_repo(
        &[
            ("src/a.py", "def shared_leaf():\n    return 1\n"),
            ("src/b.py", "def shared_leaf():\n    return 2\n"),
        ],
        &[
            ("src/a.py", "# deleted\n"),
            ("src/b.py", "def shared_leaf():\n    return 2\n"),
            ("src/c.py", "def shared_leaf():\n    return 3\n"),
        ],
    );
    let facts = run_replay(&tmp, &t0_sha, false);
    // Facts for src/a.py's shared_leaf — the one that lost its definition.
    let foo = facts
        .iter()
        .find(|f| f.fact_id.contains("src.a.shared_leaf"))
        .expect("src.a.shared_leaf fact");
    assert!(
        matches!(foo.label, Label::NeedsRevalidation),
        "expected NeedsRevalidation for module-level leaf collision; got {:?}",
        foo.label
    );
}

/// Scenario 6: byte-identical target file → Valid.
///
/// `src/a.py` is unchanged between T₀ and HEAD (triggers the
/// `file_byte_identical` bypass in `run_inner`, which labels every fact at
/// that path `Valid` without per-fact matching). A second file `src/other.py`
/// changes to give git something to commit.
#[test]
fn identity_routes_to_valid() {
    let (tmp, t0_sha, _) = build_synthetic_python_repo(
        &[
            ("src/a.py", "def foo():\n    return 1\n"),
            ("src/other.py", "x = 1\n"),
        ],
        &[
            ("src/a.py", "def foo():\n    return 1\n"),
            ("src/other.py", "x = 2\n"),
        ],
    );
    let facts = run_replay(&tmp, &t0_sha, true);
    // All facts anchored at src/a.py must be Valid (file_byte_identical bypass).
    let a_facts: Vec<_> = facts
        .iter()
        .filter(|f| f.fact_id.contains("src.a.") || f.fact_id.contains("src/a.py"))
        .collect();
    assert!(
        !a_facts.is_empty(),
        "expected at least one fact for src/a.py; got facts: {:?}",
        facts.iter().map(|f| &f.fact_id).collect::<Vec<_>>()
    );
    for f in &a_facts {
        assert!(
            matches!(f.label, Label::Valid),
            "expected Valid for byte-identical file; fact {} got {:?}",
            f.fact_id,
            f.label
        );
    }
}

/// Scenario 7: TestAssertion ordinal disambiguates siblings.
///
/// `test_thing` contains two assertions. One is unchanged (ordinal 0);
/// the second is mutated (ordinal 1). Ordinal-based pairing should produce
/// exactly 1 Valid and 1 StaleSourceChanged for the TestAssertion facts.
///
/// Filter: only `TestAssertion::` facts are examined (the FunctionSignature
/// and PublicSymbol for `test_thing` carry only the header span, which is
/// unchanged, so they also appear as Valid — they are excluded here).
#[test]
fn test_assertion_ordinal_disambiguates_siblings() {
    let (tmp, t0_sha, _) = build_synthetic_python_repo(
        &[(
            "tests/test_x.py",
            "def test_thing():\n    assert 1 == 1\n    assert 2 == 2\n",
        )],
        &[(
            "tests/test_x.py",
            "def test_thing():\n    assert 1 == 1\n    assert 2 == 3\n",
        )],
    );
    let facts = run_replay(&tmp, &t0_sha, true);
    // Filter to TestAssertion facts only (excludes FunctionSignature / PublicSymbol).
    let test_facts: Vec<_> = facts
        .iter()
        .filter(|f| f.fact_id.starts_with("TestAssertion::") && f.fact_id.contains("test_thing"))
        .collect();
    assert!(
        test_facts.len() >= 2,
        "expected ≥2 TestAssertion facts for test_thing; got {}: {:?}",
        test_facts.len(),
        test_facts.iter().map(|f| &f.fact_id).collect::<Vec<_>>()
    );
    let valid_count = test_facts
        .iter()
        .filter(|f| matches!(f.label, Label::Valid))
        .count();
    let stale_count = test_facts
        .iter()
        .filter(|f| matches!(f.label, Label::StaleSourceChanged))
        .count();
    assert_eq!(
        (valid_count, stale_count),
        (1, 1),
        "expected (1 Valid, 1 StaleSourceChanged) for one-unchanged + one-changed \
         assertion siblings; labels: {:?}",
        test_facts
            .iter()
            .map(|f| (&f.fact_id, &f.label))
            .collect::<Vec<_>>()
    );
}
