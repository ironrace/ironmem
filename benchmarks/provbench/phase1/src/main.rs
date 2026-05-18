use anyhow::Result;
use clap::{Parser, Subcommand};
use std::fs;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "provbench-phase1", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Score a baseline run's eval subset with the structural rule chain.
    /// Reads facts.jsonl + per-commit diff artifacts + baseline-run
    /// predictions.jsonl, writes predictions.jsonl + rule_traces.jsonl +
    /// phase1.sqlite + a one-line summary on stderr.
    Score {
        /// Path to the local git checkout (gix HEAD-only reader). The T0
        /// commit must be reachable here.
        #[arg(long)]
        repo: PathBuf,

        /// T0 commit SHA — the labeler's anchor commit; phase1 reads facts
        /// against this commit's tree.
        #[arg(long)]
        t0: String,

        /// Path to <repo>.facts.jsonl (output of `provbench-labeler emit-facts`).
        #[arg(long)]
        facts: PathBuf,

        /// Directory of per-commit <sha>.json diff artifacts
        /// (output of `provbench-labeler emit-diffs`).
        #[arg(long = "diffs-dir")]
        diffs_dir: PathBuf,

        /// Directory containing the LLM baseline run (must include
        /// predictions.jsonl, manifest.json, metrics.json, run_meta.json).
        /// Phase 1 evaluates exactly the row set in this baseline's
        /// predictions.jsonl.
        #[arg(long = "baseline-run")]
        baseline_run: PathBuf,

        /// Output directory; will contain predictions.jsonl, rule_traces.jsonl,
        /// and phase1.sqlite.
        #[arg(long)]
        out: PathBuf,

        /// Rule-set version label embedded in request_id and recorded in
        /// run_meta.json. Bump when rule semantics change.
        #[arg(long, default_value = "v1.3")]
        rule_set_version: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Score {
            repo,
            t0,
            facts,
            diffs_dir,
            baseline_run,
            out,
            rule_set_version,
        } => {
            fs::create_dir_all(&out)?;
            let db = provbench_phase1::storage::open(&out.join("phase1.sqlite"))?;
            provbench_phase1::facts::ingest(&db, &facts)?;
            provbench_phase1::diffs::ingest(&db, &diffs_dir)?;
            provbench_phase1::baseline_run::ingest(&db, &baseline_run.join("predictions.jsonl"))?;
            let repo = provbench_phase1::repo::Repo::open(&repo)?;
            let stats = provbench_phase1::runner::run(provbench_phase1::runner::RunnerOpts {
                db: &db,
                repo: &repo,
                t0: &t0,
                rule_set_version: &rule_set_version,
                out_predictions: &out.join("predictions.jsonl"),
                out_traces: &out.join("rule_traces.jsonl"),
            })?;
            eprintln!("phase1 done: {:?}", stats);
            Ok(())
        }
    }
}
