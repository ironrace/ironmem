use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

use abeval::corpus::Task;

#[derive(Parser, Debug)]
#[command(name = "abeval", about = "A/B eval harness (METRICS_SPEC §11)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Validate the frozen corpus and print its content hash.
    Validate {
        #[arg(long, default_value = abeval::constants::DEFAULT_CORPUS_PATH)]
        corpus: String,
    },
    /// Run one task (`--task`) OR a contiguous batch of the corpus
    /// (`--batch <index>`, size `--batch-size`, default 2) across one/both arms.
    /// Dry-run by default. Batching paces heavy live runs N tasks at a time into
    /// a shared `--out` tree; aggregate later with `report --metrics-dir`.
    Run {
        #[arg(long, default_value = abeval::constants::DEFAULT_CORPUS_PATH)]
        corpus: String,
        #[arg(long, required_unless_present = "batch", conflicts_with = "batch")]
        task: Option<String>,
        /// 0-based index of the batch to run (chunks the frozen corpus order).
        #[arg(long)]
        batch: Option<usize>,
        /// Tasks per batch (only meaningful with `--batch`).
        #[arg(long, default_value_t = 2)]
        batch_size: usize,
        #[arg(long, default_value = "both")]
        arms: String,
        #[arg(long, conflicts_with = "execute_live")]
        dry_run: bool,
        #[arg(long)]
        execute_live: bool,
        #[arg(long, name = "budget-usd")]
        budget_usd: Option<f64>,
        #[arg(long)]
        approval_file: Option<std::path::PathBuf>,
        #[arg(long)]
        out: String,
        #[arg(long)]
        base_sha: Option<String>,
    },
    /// Summarize a smoke run directory, a single metrics file, OR an aggregated
    /// `--out` tree; enforce the §11.3 headline gate. Exactly one of --run /
    /// --metrics / --metrics-dir is required.
    Report {
        /// Smoke run directory produced by `run`.
        #[arg(long, conflicts_with_all = ["metrics", "metrics_dir"])]
        run: Option<String>,
        /// A single normalized metrics file, e.g. live evidence.
        #[arg(long, conflicts_with = "metrics_dir")]
        metrics: Option<String>,
        /// An `--out` tree of per-task `live_metrics.json` files to aggregate
        /// (the batched-run output). Unions all task rows into one live report.
        #[arg(long)]
        metrics_dir: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Validate { corpus } => {
            let tasks = abeval::corpus::load_corpus(&corpus)?;
            abeval::corpus::validate_corpus(&tasks)?;
            println!("corpus OK: {} tasks", tasks.len());
            println!("content_hash: {}", abeval::corpus::content_hash(&tasks));
            Ok(())
        }
        Command::Run {
            corpus,
            task,
            batch,
            batch_size,
            arms,
            dry_run,
            execute_live,
            budget_usd,
            approval_file,
            out,
            base_sha,
        } => {
            let tasks = abeval::corpus::load_corpus(&corpus)?;
            abeval::corpus::validate_corpus(&tasks)?;
            let selected = select_run_tasks(tasks, task, batch, batch_size)?;
            // Default to dry-run unless --execute-live was explicitly passed.
            let dry = dry_run || !execute_live;
            run_selected(RunBatch {
                selected,
                arms: &arms,
                dry,
                execute_live,
                budget_usd,
                approval_file,
                out: &out,
                base_sha,
            })
        }
        Command::Report {
            run,
            metrics,
            metrics_dir,
        } => {
            let input = match report_source(run, metrics, metrics_dir)? {
                ReportSource::RunDir(d) => abeval::report::metrics_from_run_dir(&d)?,
                ReportSource::MetricsFile(f) => abeval::report::load_metrics(&f)?,
                ReportSource::MetricsDir(d) => abeval::report::load_metrics_dir(&d)?,
            };
            print!("{}", abeval::report::render_report(&input));
            Ok(())
        }
    }
}

/// Resolve which corpus tasks a `run` invocation targets: a single `--task` id,
/// or the `--batch`-th chunk of size `batch_size`. clap enforces the mutual
/// exclusion; the both/neither arms are defensive bails for a hand-built
/// invocation that bypassed clap.
fn select_run_tasks(
    tasks: Vec<Task>,
    task: Option<String>,
    batch: Option<usize>,
    batch_size: usize,
) -> Result<Vec<Task>> {
    match (task, batch) {
        (Some(id), None) => {
            let t = tasks
                .into_iter()
                .find(|t| t.id == id)
                .ok_or_else(|| anyhow::anyhow!("task {id} not found in corpus"))?;
            Ok(vec![t])
        }
        (None, Some(index)) => abeval::corpus::select_batch(&tasks, batch_size, index),
        (Some(_), Some(_)) => anyhow::bail!("pass either --task or --batch, not both"),
        (None, None) => anyhow::bail!("provide --task <id> or --batch <index>"),
    }
}

/// The evidence source a `report` invocation reads.
enum ReportSource {
    RunDir(String),
    MetricsFile(String),
    MetricsDir(String),
}

/// Validate that exactly one report source is present. clap enforces mutual
/// exclusion but cannot express "required exactly-one-of-three", so the
/// none-present case is validated here rather than silently dispatching.
fn report_source(
    run: Option<String>,
    metrics: Option<String>,
    metrics_dir: Option<String>,
) -> Result<ReportSource> {
    match (run, metrics, metrics_dir) {
        (Some(d), None, None) => Ok(ReportSource::RunDir(d)),
        (None, Some(f), None) => Ok(ReportSource::MetricsFile(f)),
        (None, None, Some(d)) => Ok(ReportSource::MetricsDir(d)),
        _ => anyhow::bail!(
            "provide exactly one of --run <dir>, --metrics <file>, or --metrics-dir <dir>"
        ),
    }
}

/// Parameters for running one selection (single task or a batch).
struct RunBatch<'a> {
    selected: Vec<Task>,
    arms: &'a str,
    dry: bool,
    execute_live: bool,
    budget_usd: Option<f64>,
    approval_file: Option<PathBuf>,
    out: &'a str,
    base_sha: Option<String>,
}

/// Run each selected task independently into its own `<out>/<task_id>/` subtree.
///
/// Batch policy: tasks are independent and a batch is resumable, so a failing
/// task does NOT strand the rest — every task is attempted, each outcome is
/// printed, and a final ledger reports which ran and which failed. The process
/// still exits non-zero if any task failed, so a partial paid batch is never
/// silently reported as success (the spent-vs-skipped boundary stays visible).
fn run_selected(b: RunBatch) -> Result<()> {
    let total = b.selected.len();
    let mut ran: Vec<String> = Vec::new();
    let mut failed: Vec<String> = Vec::new();
    for selected_task in b.selected {
        let task_id = selected_task.id.clone();
        // Base-commit precedence (including the illegal "override a pin" state)
        // is enforced by the single authority `resolve_base_commit`, reached via
        // the live executor; no duplicate guard here. The live cost-approval
        // gate is re-checked per task inside `run_task`.
        let result = abeval::arms::assign_arms(&task_id, b.arms).and_then(|arm_list| {
            abeval::runner::run_task(abeval::runner::RunArgs {
                task: selected_task,
                arms: arm_list,
                dry_run: b.dry,
                execute_live: b.execute_live,
                budget_usd: b.budget_usd,
                approval_file: b.approval_file.clone(),
                out_dir: PathBuf::from(b.out),
                base_sha: b.base_sha.clone(),
            })
        });
        match result {
            Ok(summary) => {
                println!("ran {} ({} arms)", summary.task_id, summary.arms_run);
                ran.push(task_id);
            }
            Err(e) => {
                // Full error chain to stderr so a per-task failure is never lost.
                eprintln!("FAILED {task_id}: {e:#}");
                failed.push(task_id);
            }
        }
    }
    if total > 1 || !failed.is_empty() {
        let failed_note = if failed.is_empty() {
            String::new()
        } else {
            format!("; failed [{}]", failed.join(", "))
        };
        println!(
            "batch summary: {}/{total} ran [{}]{failed_note}",
            ran.len(),
            ran.join(", "),
        );
    }
    if !failed.is_empty() {
        anyhow::bail!(
            "{} of {total} task(s) failed: {}",
            failed.len(),
            failed.join(", ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{report_source, select_run_tasks, Cli, ReportSource};
    use clap::{CommandFactory, Parser};

    /// Guards every `conflicts_with*` / `required_unless_present` id against the
    /// arg ids that actually exist — the check that would have caught the
    /// `name = "metrics-dir"` id-rename breaking `conflicts_with = "metrics_dir"`.
    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }

    // --- run: --task / --batch mutual exclusion enforced by clap at parse time ---

    #[test]
    fn run_rejects_task_and_batch_together() {
        let r = Cli::try_parse_from(["abeval", "run", "--task", "x", "--batch", "0", "--out", "o"]);
        assert!(r.is_err(), "clap must reject --task with --batch");
    }

    #[test]
    fn run_requires_task_or_batch() {
        let r = Cli::try_parse_from(["abeval", "run", "--out", "o"]);
        assert!(r.is_err(), "clap must require one of --task / --batch");
    }

    #[test]
    fn run_accepts_batch_alone() {
        let r = Cli::try_parse_from(["abeval", "run", "--batch", "0", "--out", "o"]);
        assert!(r.is_ok(), "--batch alone is valid: {r:?}");
    }

    // --- report: exactly-one-of-three source selection ---

    #[test]
    fn report_rejects_multiple_sources_at_parse_time() {
        let r = Cli::try_parse_from(["abeval", "report", "--metrics", "a", "--metrics-dir", "b"]);
        assert!(r.is_err(), "clap must reject --metrics with --metrics-dir");
    }

    #[test]
    fn report_source_requires_exactly_one() {
        // The none-provided case clap cannot express; validated in code.
        assert!(report_source(None, None, None).is_err());
        // Defensive: a hand-built both-present pair is rejected.
        assert!(report_source(Some("a".into()), Some("b".into()), None).is_err());
        assert!(matches!(
            report_source(None, None, Some("d".into())).unwrap(),
            ReportSource::MetricsDir(_)
        ));
    }

    // --- run task selection (pure dispatch helper) ---

    #[test]
    fn select_run_tasks_defensive_bails() {
        // Neither and both are defensive errors (clap normally prevents them).
        assert!(select_run_tasks(vec![], None, None, 2).is_err());
        assert!(select_run_tasks(vec![], Some("x".into()), Some(0), 2).is_err());
    }

    #[test]
    fn select_run_tasks_rejects_unknown_task_id() {
        // Empty corpus exercises the not-found path without constructing a Task.
        let err = select_run_tasks(vec![], Some("nope".into()), None, 2)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "got: {err}");
    }
}
