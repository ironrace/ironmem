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
    /// Run one task across one/both arms (dry-run by default).
    Run {
        #[arg(long, default_value = abeval::constants::DEFAULT_CORPUS_PATH)]
        corpus: String,
        #[arg(long)]
        task: String,
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
        /// Smoke run directory produced by `run` (mutually exclusive with --metrics).
        #[arg(long, required_unless_present = "metrics", conflicts_with = "metrics")]
        run: Option<String>,
        /// Normalized metrics file, e.g. live evidence (mutually exclusive with --run).
        #[arg(long)]
        metrics: Option<String>,
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
            let selected = tasks
                .into_iter()
                .find(|t| t.id == task)
                .ok_or_else(|| anyhow::anyhow!("task {task} not found in corpus"))?;
            if base_sha.is_some() && !selected.base_commit.trim().is_empty() {
                anyhow::bail!(
                    "--base-sha cannot override pinned base_commit for task {}; \
                     edit the corpus pin intentionally instead",
                    selected.id
                );
            }
            let arm_list = abeval::arms::assign_arms(&selected.id, &arms)?;
            // Default to dry-run unless --execute-live was explicitly passed.
            let dry = dry_run || !execute_live;
            let summary = abeval::runner::run_task(abeval::runner::RunArgs {
                task: selected,
                arms: arm_list,
                dry_run: dry,
                execute_live,
                budget_usd,
                approval_file,
                out_dir: std::path::PathBuf::from(out),
                base_sha,
            })?;
            println!("ran {} ({} arms)", summary.task_id, summary.arms_run);
            Ok(())
        }
        Command::Report { run, metrics } => {
            // clap enforces exactly-one-of via required_unless_present +
            // conflicts_with; the final arm is an unreachable safety bail.
            let input = match (run, metrics) {
                (_, Some(metrics_path)) => abeval::report::load_metrics(&metrics_path)?,
                (Some(run_dir), None) => abeval::report::metrics_from_run_dir(&run_dir)?,
                (None, None) => {
                    anyhow::bail!("provide exactly one of --run <dir> or --metrics <file>")
                }
            };
            print!("{}", abeval::report::render_report(&input));
            Ok(())
        }
    }
}
