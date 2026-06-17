//! Corpus batching (`select_batch`): pace heavy live runs N tasks at a time.

use abeval::corpus::{select_batch, BaseCommit, Task};

fn task(id: &str) -> Task {
    Task {
        id: id.to_string(),
        title: id.to_string(),
        source: "issue:#1".to_string(),
        repo_scope: vec!["x/**".to_string()],
        prompt: "p".to_string(),
        acceptance: vec!["ok".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
        base_commit: BaseCommit::parse("ce2b27f").unwrap(),
    }
}

fn corpus(n: usize) -> Vec<Task> {
    (0..n).map(|i| task(&format!("abeval-{i:02}"))).collect()
}

fn ids(tasks: &[Task]) -> Vec<String> {
    tasks.iter().map(|t| t.id.clone()).collect()
}

#[test]
fn batch_returns_contiguous_chunk_of_size() {
    let c = corpus(8);
    assert_eq!(
        ids(&select_batch(&c, 2, 0).unwrap()),
        ["abeval-00", "abeval-01"]
    );
    assert_eq!(
        ids(&select_batch(&c, 2, 1).unwrap()),
        ["abeval-02", "abeval-03"]
    );
    assert_eq!(
        ids(&select_batch(&c, 2, 3).unwrap()),
        ["abeval-06", "abeval-07"]
    );
}

#[test]
fn final_batch_may_be_shorter_than_size() {
    let c = corpus(5); // 3 batches of 2: [0,1] [2,3] [4]
    assert_eq!(ids(&select_batch(&c, 2, 2).unwrap()), ["abeval-04"]);
}

#[test]
fn batches_partition_the_whole_corpus_without_loss_or_overlap() {
    let c = corpus(8);
    let mut seen: Vec<String> = Vec::new();
    for i in 0..4 {
        seen.extend(ids(&select_batch(&c, 2, i).unwrap()));
    }
    assert_eq!(seen, ids(&c)); // union, in order, no dupes
}

#[test]
fn out_of_range_index_is_a_loud_error() {
    let c = corpus(8); // valid indices 0..4
    let err = select_batch(&c, 2, 4).unwrap_err().to_string();
    assert!(err.contains("out of range"), "got: {err}");
    assert!(err.contains('4'), "message should state batch count: {err}");
}

#[test]
fn zero_batch_size_is_rejected() {
    let c = corpus(8);
    assert!(select_batch(&c, 0, 0).is_err());
}

#[test]
fn batch_size_larger_than_corpus_returns_all_as_batch_zero() {
    let c = corpus(3);
    assert_eq!(
        ids(&select_batch(&c, 10, 0).unwrap()),
        ["abeval-00", "abeval-01", "abeval-02"]
    );
    // Only one batch exists (div_ceil(3, 10) == 1), so index 1 is out of range.
    assert!(select_batch(&c, 10, 1).is_err());
}

#[test]
fn single_element_corpus_has_exactly_one_batch() {
    let c = corpus(1);
    assert_eq!(ids(&select_batch(&c, 2, 0).unwrap()), ["abeval-00"]);
    assert!(select_batch(&c, 2, 1).is_err());
}

#[test]
fn batch_size_one_maps_each_index_to_a_single_task() {
    let c = corpus(3);
    assert_eq!(ids(&select_batch(&c, 1, 0).unwrap()), ["abeval-00"]);
    assert_eq!(ids(&select_batch(&c, 1, 1).unwrap()), ["abeval-01"]);
    assert_eq!(ids(&select_batch(&c, 1, 2).unwrap()), ["abeval-02"]);
    assert!(select_batch(&c, 1, 3).is_err()); // num_batches == len == 3
}

#[test]
fn empty_corpus_is_rejected() {
    let c: Vec<Task> = Vec::new();
    assert!(select_batch(&c, 2, 0).is_err());
}
