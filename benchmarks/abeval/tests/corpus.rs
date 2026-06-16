use abeval::corpus::{content_hash, load_corpus, validate_corpus, Task};
use std::process::Command;

fn sample_task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        title: "t".to_string(),
        source: "issue:#95".to_string(),
        repo_scope: vec!["crates/**".to_string()],
        prompt: "do the thing".to_string(),
        acceptance: vec!["it works".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
    }
}

fn tasks(n: usize) -> Vec<Task> {
    (0..n)
        .map(|i| sample_task(&format!("abeval-{i:02}-x")))
        .collect()
}

#[test]
fn accepts_eight_to_twelve_tasks() {
    assert!(validate_corpus(&tasks(8)).is_ok());
    assert!(validate_corpus(&tasks(12)).is_ok());
}

#[test]
fn rejects_below_minimum_and_above_maximum() {
    assert!(validate_corpus(&tasks(7)).is_err());
    assert!(validate_corpus(&tasks(13)).is_err());
}

#[test]
fn rejects_duplicate_ids() {
    let mut t = tasks(8);
    t[1].id = t[0].id.clone();
    assert!(validate_corpus(&t).is_err());
}

#[test]
fn rejects_empty_id() {
    let mut t = tasks(8);
    t[0].id = String::new();
    assert!(validate_corpus(&t).is_err());
}

#[test]
fn rejects_path_unsafe_ids() {
    for id in [
        "../escape",
        "/tmp/escape",
        "nested/path",
        "C:\\escape",
        "has space",
    ] {
        let mut t = tasks(8);
        t[0].id = id.to_string();
        assert!(
            validate_corpus(&t).is_err(),
            "expected unsafe id {id:?} to be rejected"
        );
    }
}

#[test]
fn rejects_missing_acceptance_or_gate() {
    let mut t = tasks(8);
    t[0].acceptance.clear();
    assert!(validate_corpus(&t).is_err());

    let mut t2 = tasks(8);
    t2[0].gates.clear();
    assert!(validate_corpus(&t2).is_err());
}

#[test]
fn rejects_synthetic_source() {
    let mut t = tasks(8);
    t[0].source = "synthetic:puzzle-1".to_string();
    assert!(validate_corpus(&t).is_err());
}

#[test]
fn content_hash_is_stable_across_loads() {
    let t = tasks(8);
    assert_eq!(content_hash(&t), content_hash(&t));
}

#[test]
fn committed_corpus_validates() {
    let t = load_corpus("corpus/tasks.jsonl").expect("load committed corpus");
    validate_corpus(&t).expect("committed corpus must validate");
    assert!(!content_hash(&t).is_empty());
}

#[test]
fn validate_default_corpus_works_from_repo_root() {
    let repo_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("crate lives under benchmarks/abeval");
    let output = Command::new(env!("CARGO_BIN_EXE_abeval"))
        .arg("validate")
        .current_dir(repo_root)
        .output()
        .expect("run abeval validate");

    assert!(
        output.status.success(),
        "validate failed\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("content_hash:"));
}
