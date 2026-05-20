//! `provbench-baseline` CLI entry point.
//!
//! Subcommands are skeletons; implementations land in subsequent tasks:
//!   - `sample` — Task 5 (deterministic mutation sampling)
//!   - `run`    — Task 8 (LLM invalidation pass over sampled mutations)
//!   - `score`  — Task 9 (three-way scoring vs labeler ground truth)

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "provbench-baseline",
    version,
    about = "Phase 0c LLM-as-invalidator baseline for ProvBench-CodeContext"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Deterministically sample mutations from labeler artifacts (Task 5).
    Sample(SampleArgs),
    /// Execute the LLM invalidator over sampled mutations (Task 8).
    Run(RunArgs),
    /// Score baseline decisions against labeler ground truth (Task 9).
    Score(ScoreArgs),
}

#[derive(Debug, clap::Args)]
struct SampleArgs {
    /// Path to the labeler corpus JSONL (e.g. `…/corpus/<run>.jsonl`).
    #[arg(long)]
    corpus: PathBuf,
    /// Path to the matching `*.facts.jsonl` artifact.
    #[arg(long)]
    facts: PathBuf,
    /// Path to the matching `*.diffs/` directory of per-commit JSON
    /// artifacts.
    #[arg(long)]
    diffs_dir: PathBuf,
    /// PRNG seed for the deterministic per-stratum draw.
    #[arg(long, default_value_t = provbench_baseline::constants::DEFAULT_SEED)]
    seed: u64,
    /// Operational budget ceiling in USD. Preflight refuses if the
    /// worst-case estimate exceeds this.
    #[arg(long, default_value_t = provbench_baseline::constants::DEFAULT_OPERATIONAL_BUDGET_USD)]
    budget_usd: f64,
    /// Path to write the resulting `manifest.json`.
    #[arg(long)]
    out: PathBuf,
    /// Per-stratum target for the `Valid` class. Defaults to the
    /// pre-registered §9.2 constant. Exposes a knob (sample shape),
    /// not a rule/threshold — does NOT consume the §10 anti-leakage
    /// clock.
    #[arg(long, default_value_t = provbench_baseline::constants::TARGET_VALID)]
    target_valid: usize,
    /// Per-stratum target for the `StaleSourceChanged` class.
    #[arg(long, default_value_t = provbench_baseline::constants::TARGET_STALE_CHANGED)]
    target_stale_source_changed: usize,
    /// Per-stratum target for the `StaleSourceDeleted` class.
    #[arg(long, default_value_t = provbench_baseline::constants::TARGET_STALE_DELETED)]
    target_stale_source_deleted: usize,
    /// Per-stratum target for the `StaleSymbolRenamed` class.
    /// Sentinel `usize::MAX` means "take the entire stratum".
    #[arg(long, default_value_t = provbench_baseline::constants::TARGET_STALE_RENAMED)]
    target_stale_symbol_renamed: usize,
    /// Per-stratum target for the `NeedsRevalidation` class.
    #[arg(long, default_value_t = provbench_baseline::constants::TARGET_NEEDS_REVALIDATION)]
    target_needs_revalidation: usize,
}

#[derive(Debug, clap::Args)]
struct RunArgs {
    /// Path to the `manifest.json` minted by `sample`.
    #[arg(long)]
    manifest: PathBuf,
    /// Forward-compat knob; v1 dispatches sequentially.
    #[arg(long, default_value_t = 1)]
    max_concurrency: usize,
    /// Resume from `<run_dir>/predictions.jsonl`. Verifies the on-disk
    /// manifest's `content_hash` matches the in-memory manifest first.
    #[arg(long, default_value_t = false)]
    resume: bool,
    /// Skip the live Anthropic call — fabricate a "valid" decision per
    /// row. Intended for CI and the resume-safety test.
    #[arg(long, default_value_t = false)]
    dry_run: bool,
    /// Replay batch responses from `<dir>/<batch_id>.json` instead of
    /// calling the API. Useful for offline development.
    #[arg(long)]
    fixture_mode: Option<PathBuf>,
    /// Bound the number of batches dispatched this invocation. Off by
    /// default; primarily for smoke runs.
    #[arg(long)]
    max_batches: Option<usize>,
    /// Operational budget ceiling in USD enforced by the runtime cost
    /// meter (separate from the preflight refusal cap in `sample`).
    /// Must remain ≤ the immutable SPEC §6.2 / §15 ceiling.
    #[arg(long, name = "budget-usd", default_value_t = provbench_baseline::constants::DEFAULT_OPERATIONAL_BUDGET_USD)]
    budget_usd: f64,
    /// Sliding-window input-tokens-per-minute throttle. `0` disables
    /// the throttle entirely. Default is well under Anthropic's 450k
    /// ITPM org-cap on `claude-sonnet-4-6` to leave headroom for the
    /// parse-retry doublings. Robustness knob only — does not touch
    /// the §10 frozen surface.
    #[arg(long, default_value_t = provbench_baseline::constants::DEFAULT_MAX_INPUT_TOKENS_PER_MINUTE)]
    max_input_tokens_per_minute: usize,
}

#[derive(Debug, clap::Args)]
struct ScoreArgs {
    /// Run directory containing `manifest.json` + `predictions.jsonl`
    /// (and optionally `run_meta.json`). `metrics.json` is written here.
    #[arg(long)]
    run: PathBuf,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Sample(args) => {
            let targets = provbench_baseline::sample::PerStratumTargets {
                valid: args.target_valid,
                stale_changed: args.target_stale_source_changed,
                stale_deleted: args.target_stale_source_deleted,
                stale_renamed: args.target_stale_symbol_renamed,
                needs_revalidation: args.target_needs_revalidation,
            };
            let manifest = provbench_baseline::manifest::SampleManifest::from_corpus(
                &args.corpus,
                &args.facts,
                &args.diffs_dir,
                args.seed,
                targets,
                args.budget_usd,
            )?;
            manifest.save_atomic(&args.out)?;
            println!(
                "wrote manifest to {} (selected={}, excluded={})",
                args.out.display(),
                manifest.selected_count,
                manifest.excluded_count_by_reason.values().sum::<usize>()
            );
            Ok(())
        }
        Command::Run(args) => {
            let manifest: provbench_baseline::manifest::SampleManifest =
                serde_json::from_slice(&std::fs::read(&args.manifest)?)?;
            let run_dir = args
                .manifest
                .parent()
                .ok_or_else(|| anyhow::anyhow!("manifest path has no parent"))?
                .to_path_buf();
            let result = tokio::runtime::Runtime::new()?.block_on(
                provbench_baseline::runner::run(provbench_baseline::runner::RunnerOpts {
                    run_dir,
                    manifest,
                    budget_usd: args.budget_usd,
                    resume: args.resume,
                    dry_run: args.dry_run,
                    fixture_mode: args.fixture_mode,
                    max_batches: args.max_batches,
                    max_concurrency: args.max_concurrency,
                    max_input_tokens_per_minute: args.max_input_tokens_per_minute,
                    client_override: None,
                }),
            )?;
            println!(
                "batches: {}/{}  cost: ${:.2}  aborted: {}",
                result.batches_completed,
                result.batches_total,
                result.total_cost_usd,
                result.aborted
            );
            if result.aborted {
                std::process::exit(2);
            }
            Ok(())
        }
        Command::Score(args) => {
            provbench_baseline::report::score_run(&args.run)?;
            let out = args.run.join("metrics.json");
            println!("wrote metrics to {}", out.display());
            Ok(())
        }
    }
}
