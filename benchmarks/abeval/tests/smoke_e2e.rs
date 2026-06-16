use abeval::arms::Arm;
use abeval::corpus::load_corpus;
use abeval::report::{metrics_from_run_dir, render_report};
use abeval::runner::{run_task, RunArgs};

#[test]
fn one_corpus_task_both_arms_dry_run_then_report() {
    let tasks = load_corpus("corpus/tasks.jsonl").expect("load committed corpus");
    let task = tasks.into_iter().next().expect("corpus non-empty");

    let dir = tempfile::tempdir().unwrap();
    let summary = run_task(RunArgs {
        task: task.clone(),
        arms: vec![Arm::Ironmem, Arm::Superpowers],
        dry_run: true,
        execute_live: false,
        budget_usd: None,
        out_dir: dir.path().to_path_buf(),
    })
    .unwrap();
    assert_eq!(summary.arms_run, 2);

    // Both arm artifacts exist.
    for arm in ["ironmem", "superpowers"] {
        assert!(dir.path().join(&task.id).join(arm).join("usage.json").exists());
    }

    // Report renders a non-headline (smoke) summary.
    let input = metrics_from_run_dir(dir.path()).unwrap();
    let out = render_report(&input);
    assert!(out.contains("SMOKE"));
    assert!(!out.contains("DELTA"));
}
