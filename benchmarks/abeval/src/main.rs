use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "abeval", about = "A/B eval harness (METRICS_SPEC §11)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
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
    /// Summarize a run directory OR a normalized metrics file; enforce the
    /// §11.3 headline gate. Exactly one of --run / --metrics is required.
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
            // clap guarantees exactly one of --task / --batch; the remaining
            // arms are defensive bails for a hand-built invocation.
            let selected = match (task, batch) {
                (Some(id), None) => {
                    let t = tasks
                        .into_iter()
                        .find(|t| t.id == id)
                        .ok_or_else(|| anyhow::anyhow!("task {id} not found in corpus"))?;
                    vec![t]
                }
                (None, Some(index)) => abeval::corpus::select_batch(&tasks, batch_size, index)?,
                (Some(_), Some(_)) => anyhow::bail!("pass either --task or --batch, not both"),
                (None, None) => anyhow::bail!("provide --task <id> or --batch <index>"),
            };
            // Default to dry-run unless --execute-live was explicitly passed.
            let dry = dry_run || !execute_live;
            // Each task in a batch runs independently into its own
            // `<out>/<task_id>/` subtree; the live cost-approval gate is
            // re-checked per task inside `run_task`.
            for selected_task in selected {
                // Base-commit precedence (including the illegal "override a pin"
                // state) is enforced by the single authority `resolve_base_commit`,
                // reached via the live executor; no duplicate guard here.
                let arm_list = abeval::arms::assign_arms(&selected_task.id, &arms)?;
                let summary = abeval::runner::run_task(abeval::runner::RunArgs {
                    task: selected_task,
                    arms: arm_list,
                    dry_run: dry,
                    execute_live,
                    budget_usd,
                    approval_file: approval_file.clone(),
                    out_dir: std::path::PathBuf::from(&out),
                    base_sha: base_sha.clone(),
                })?;
                println!("ran {} ({} arms)", summary.task_id, summary.arms_run);
            }
            Ok(())
        }
        Command::Report {
            run,
            metrics,
            metrics_dir,
        } => {
            // clap enforces mutual exclusion; the final arm is the
            // none-provided error (clap can't require exactly-one of three).
            let input = match (run, metrics, metrics_dir) {
                (Some(run_dir), None, None) => abeval::report::metrics_from_run_dir(&run_dir)?,
                (None, Some(metrics_path), None) => abeval::report::load_metrics(&metrics_path)?,
                (None, None, Some(dir)) => abeval::report::load_metrics_dir(&dir)?,
                _ => anyhow::bail!(
                    "provide exactly one of --run <dir>, --metrics <file>, or --metrics-dir <dir>"
                ),
            };
            print!("{}", abeval::report::render_report(&input));
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Cli;
    use clap::CommandFactory;

    /// Guards every `conflicts_with*` / `required_unless_present` id against the
    /// arg ids that actually exist — the check that would have caught the
    /// `name = "metrics-dir"` id-rename breaking `conflicts_with = "metrics_dir"`.
    #[test]
    fn cli_definition_is_internally_consistent() {
        Cli::command().debug_assert();
    }
}
