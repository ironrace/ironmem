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
        out: String,
    },
    /// Summarize a run directory; enforce the §11.3 headline gate.
    Report {
        #[arg(long)]
        run: String,
        #[arg(long)]
        metrics: Option<String>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Validate { .. } => unimplemented!("wired in Task 2"),
        Command::Run { .. } => unimplemented!("wired in Task 4/5"),
        Command::Report { .. } => unimplemented!("wired in Task 6"),
    }
}
