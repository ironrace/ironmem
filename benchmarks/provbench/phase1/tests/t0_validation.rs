//! Regression: `Repo::verify_commit` must error loudly when --t0 doesn't
//! resolve. Without this, the runner silently produces wrong predictions
//! (every t0_blob read returns None and rules fall through). See
//! `runner::run` entry validation.

use provbench_phase1::repo::Repo;
use std::path::Path;

#[test]
fn verify_commit_errors_on_unknown_sha() {
    // Use an existing serde checkout; pick a SHA that obviously doesn't exist.
    let repo_path = Path::new("../work/serde");
    if !repo_path.exists() {
        eprintln!("skipping: work/serde not present");
        return;
    }
    let repo = Repo::open(repo_path).expect("open serde checkout");
    let result = repo.verify_commit("65e1a5076d6e29cee76346d49ec632a8a1e63aa3");
    assert!(
        result.is_err(),
        "expected error on nonexistent commit, got {:?}",
        result
    );
    let msg = format!("{:?}", result.unwrap_err());
    assert!(
        msg.contains("65e1a5076d") || msg.to_lowercase().contains("commit"),
        "error message should reference the offending sha or 'commit'; got: {msg}"
    );
}

#[test]
fn verify_commit_passes_on_real_sha() {
    let repo_path = Path::new("../work/serde");
    if !repo_path.exists() {
        eprintln!("skipping: work/serde not present");
        return;
    }
    let repo = Repo::open(repo_path).expect("open serde checkout");
    // T0 used by the v1.1/v1.3 serde rounds.
    let result = repo.verify_commit("65e1a50749938612cfbdb69b57fc4cf249f87149");
    assert!(result.is_ok(), "expected ok on real T0, got {:?}", result);
}

#[test]
fn verify_commit_errors_on_malformed_sha() {
    let repo_path = Path::new("../work/serde");
    if !repo_path.exists() {
        eprintln!("skipping: work/serde not present");
        return;
    }
    let repo = Repo::open(repo_path).expect("open serde checkout");
    // Not a hex sha at all.
    let result = repo.verify_commit("not_a_real_sha");
    assert!(
        result.is_err(),
        "expected error on malformed sha, got {:?}",
        result
    );
}
