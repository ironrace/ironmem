use clap::{Parser, Subcommand};
use std::process;

use ironmem::review_diff::{build_review_diff, expand_review_diff, ReviewDiffRequest};
use ironmem::MemoryError;
use ironmem::{
    bootstrap, config, context, dashboard, ingest, launcher, mcp, migrate, reembed, report,
    symbol_graph,
};

#[derive(Parser)]
#[command(
    name = "ironmem",
    version = env!("IRONMEM_VERSION"),
    about = "AI memory — semantic search + knowledge graph"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Start the MCP server (JSON-RPC over stdio)
    Serve {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Run as the shared daemon bound to this Unix socket path
        #[arg(long)]
        listen: Option<String>,
        /// Run as a thin proxy connecting to this Unix socket path
        #[arg(long, conflicts_with = "listen")]
        connect: Option<String>,
        /// Disable daemon auto-spawn from proxy (--connect) mode
        #[arg(long)]
        no_autospawn: bool,
    },
    /// Initialize a new memory store
    Init,
    /// Download the embedding model
    Setup,
    /// Mine files into memory
    Mine {
        /// Directory to mine
        path: String,
    },
    /// Migrate from a ChromaDB store
    Migrate {
        /// Path to existing ChromaDB directory
        #[arg(long)]
        from: String,
    },
    /// Re-embed all drawers using the current model (run after a model upgrade)
    Reembed {
        /// Only re-embed drawers in this wing
        #[arg(long)]
        wing: Option<String>,
    },
    /// Run a hook (called by Claude Code / Codex)
    Hook {
        /// Hook name: stop, precompact, session-start, user-prompt-submit
        name: String,
        /// Harness: claude-code, codex
        #[arg(long, default_value = "claude-code")]
        harness: String,
    },
    /// Render the metrics report (METRICS_SPEC §10 + §7 cost)
    Report {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Only this task (collab_session_id or task_tag)
        #[arg(long)]
        task: Option<String>,
        /// Only rows at/after this RFC3339 instant or YYYY-MM-DD date (inclusive)
        #[arg(long)]
        since: Option<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Diagnose the local installation (binary, db, model, MCP mode, harnesses)
    Doctor {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Assemble a compact context pack for a task (memory + decisions + code maps)
    Context {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repository root for code-map lookup
        #[arg(long, default_value = ".")]
        repo: String,
        /// Task description driving memory recall
        #[arg(long)]
        task: String,
        /// Code-map area to include (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Approximate output token budget
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Write the ironmem memory-protocol managed block into rules file(s) (explicit opt-in)
    WriteRules {
        /// Target rules file. Omit to write all default harness rules files.
        /// Validated against registered harness rules files. Non-native selections
        /// also ensure the canonical AGENTS.md dependency is updated.
        #[arg(long, conflicts_with = "harness")]
        target: Option<String>,
        /// Write this harness's rules file (e.g. claude → CLAUDE.md, codex → AGENTS.md).
        /// Non-native harnesses also update the canonical AGENTS.md dependency.
        #[arg(long, conflicts_with = "target")]
        harness: Option<String>,
        /// Directory containing the target file(s)
        #[arg(long, default_value = ".")]
        workspace: String,
    },
    /// Launch Claude Code in a repo with the ironmem MCP server attached
    Claude {
        /// Repository path to launch in
        #[arg(default_value = ".")]
        path: String,
        /// Optional initial prompt for the session
        prompt: Option<String>,
        /// Skip ensuring the ironmem MCP server is registered (use existing manual setup)
        #[arg(long)]
        no_mcp_setup: bool,
        /// Code-map area to pre-inject context for (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Disable compact context pre-injection into the initial prompt
        #[arg(long)]
        no_context: bool,
        /// Approximate token budget for pre-injected context
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
    },
    /// Start a local read-only dashboard server for inspecting memory, code maps, sessions, and metrics
    Dashboard {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Host to bind (default: 127.0.0.1 loopback only)
        #[arg(long, default_value = "127.0.0.1")]
        host: String,
        /// Port to bind (0 = ephemeral)
        #[arg(long, default_value_t = 7384)]
        port: u16,
        /// Allow binding to a non-loopback address (WARNING: exposes dashboard to the network)
        #[arg(long)]
        allow_non_loopback: bool,
        /// Emit startup metadata as JSON instead of prose
        #[arg(long)]
        json: bool,
        /// Internal test hook: exit when stdin closes
        #[arg(long, hide = true)]
        exit_on_stdin_close: bool,
    },
    /// Build a compressed Git diff artifact or expand one of its indexed hunks
    ReviewDiff {
        /// Git repository containing the diff source
        #[arg(long, default_value = ".")]
        repo: String,
        /// Diff uncommitted worktree changes relative to HEAD
        #[arg(long, conflicts_with_all = ["base", "head"])]
        worktree: bool,
        /// Base revision for a merge-base range
        #[arg(long, conflicts_with = "worktree")]
        base: Option<String>,
        /// Head revision for a merge-base range
        #[arg(long, conflicts_with = "worktree")]
        head: Option<String>,
        /// Expand this indexed file instead of printing the compressed artifact
        #[arg(long)]
        expand_file: Option<String>,
        /// One-based hunk ordinal within --expand-file
        #[arg(long, requires = "expand_file")]
        hunk: Option<usize>,
    },
    /// Build or query the local symbol/import graph index
    Symbols {
        #[command(subcommand)]
        cmd: SymbolsCmd,
    },
    /// Inspect or prune memory lifecycle artifacts
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Onboard or approve a repo's autopilot gate config (build-ladder rung 3)
    Autopilot {
        #[command(subcommand)]
        cmd: AutopilotCmd,
    },
    /// List registered harnesses (dev/CI helper for packaging scripts)
    Harnesses {
        /// Output format
        #[arg(long, default_value = "text", value_parser = ["json", "text"])]
        format: String,
    },
    /// Launch Codex in a repo with the ironmem MCP server attached
    Codex {
        /// Repository path to launch in
        #[arg(default_value = ".")]
        path: String,
        /// Optional initial prompt for the session
        prompt: Option<String>,
        /// Skip ensuring the ironmem MCP server is registered (use existing manual setup)
        #[arg(long)]
        no_mcp_setup: bool,
        /// Code-map area to pre-inject context for (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Disable compact context pre-injection into the initial prompt
        #[arg(long)]
        no_context: bool,
        /// Approximate token budget for pre-injected context
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
    },
    /// Launch Grok in a repo with the ironmem MCP server attached (scaffolding — #190 Task 13)
    Grok {
        /// Repository path to launch in
        #[arg(default_value = ".")]
        path: String,
        /// Optional initial prompt for the session
        prompt: Option<String>,
        /// Skip ensuring the ironmem MCP server is registered (use existing manual setup)
        #[arg(long)]
        no_mcp_setup: bool,
        /// Code-map area to pre-inject context for (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Disable compact context pre-injection into the initial prompt
        #[arg(long)]
        no_context: bool,
        /// Approximate token budget for pre-injected context
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
    },
    /// Launch Gemini CLI in a repo with the ironmem MCP server attached
    Gemini {
        /// Repository path to launch in
        #[arg(default_value = ".")]
        path: String,
        /// Optional initial prompt for the session
        prompt: Option<String>,
        /// Skip ensuring the ironmem MCP server is registered (use existing manual setup)
        #[arg(long)]
        no_mcp_setup: bool,
        /// Code-map area to pre-inject context for (repeatable)
        #[arg(long = "area")]
        areas: Vec<String>,
        /// Disable compact context pre-injection into the initial prompt
        #[arg(long)]
        no_context: bool,
        /// Approximate token budget for pre-injected context
        #[arg(long, default_value_t = ironmem::context::DEFAULT_BUDGET_TOKENS)]
        budget: usize,
    },
}

/// Subcommands nested under `ironmem memory`.
#[derive(Subcommand)]
enum MemoryCmd {
    /// Dry-run or apply conservative stale-drawer garbage collection
    Gc {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Show candidates without deleting anything (the default)
        #[arg(long, conflicts_with = "apply")]
        dry_run: bool,
        /// Actually delete candidates. Omit for dry-run.
        #[arg(long)]
        apply: bool,
        /// Retention for collab-checkpoints drawers
        #[arg(long, default_value_t = ironmem::db::retention::DEFAULT_COLLAB_CHECKPOINT_RETENTION_DAYS)]
        collab_checkpoint_days: i64,
        /// Retention for collab-plans and collab-task-lists drawers
        #[arg(long, default_value_t = ironmem::db::retention::DEFAULT_COLLAB_ARTIFACT_RETENTION_DAYS)]
        collab_artifact_days: i64,
        /// Maximum candidates to inspect
        #[arg(long, default_value_t = ironmem::db::retention::DEFAULT_GC_LIMIT)]
        limit: usize,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

/// Subcommands nested under `ironmem autopilot` (spec's *Repo onboarding*
/// section, steps 1 and 3 — step 2, gate inference, is
/// `ironmem::autopilot::onboard::infer_gate_commands`; `Run` is rung 4's
/// end-to-end single-issue loop).
///
/// `large_enum_variant` is allowed rather than satisfied: `Run` carries the
/// whole `RunConfig` surface as individual clap arguments, which makes it
/// far larger than its two siblings. The lint's concern — an oversized enum
/// copied around a hot path — does not apply to a command enum that is
/// parsed exactly once at startup and immediately destructured, and clippy's
/// suggested `Box<String>` fields would obscure the argument definitions
/// without changing anything that runs.
#[derive(Subcommand)]
#[allow(clippy::large_enum_variant)]
enum AutopilotCmd {
    /// Infer gate commands for a repo checkout and write a pending proposal
    Onboard {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// Local checkout to inspect for build manifests
        #[arg(long, default_value = ".")]
        path: String,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Approve a repo's pending gate-config proposal
    Approve {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Drive one issue end to end against its approved gate config (rung 4)
    Run {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// GitHub issue number
        issue: u64,
        /// Issue title, shown to the IC in its turn prompt
        #[arg(long)]
        title: String,
        /// Issue body text. Mutually exclusive with --body-file.
        #[arg(long, conflicts_with = "body_file")]
        body: Option<String>,
        /// Read the issue body from a file instead of the command line
        #[arg(long)]
        body_file: Option<String>,
        /// Local checkout of `repo` that worktrees are cut from
        #[arg(long, default_value = ".")]
        path: String,
        /// Directory that per-issue worktrees are created under
        #[arg(long)]
        worktree_root: Option<String>,
        /// Committish new issue branches are cut from
        #[arg(long, default_value = "HEAD")]
        base: String,
        /// Model for the IC dispatch
        #[arg(long, default_value = "claude-sonnet-5")]
        model: String,
        /// Dispatch-time risk class, recorded for the Reviewer to compare against
        #[arg(long = "class", default_value = "unclassified")]
        dispatch_class: String,
        /// Turns per dispatch (the N in "or stop after N turns")
        #[arg(long)]
        n_turns: Option<u32>,
        /// Hard --max-turns ceiling; must clear --n-turns with headroom
        #[arg(long)]
        max_turns: Option<u32>,
        /// Per-dispatch spend ceiling, passed through to --max-budget-usd
        #[arg(long)]
        max_budget_usd: Option<f64>,
        /// Per-issue attempt cap, cumulative across runs
        #[arg(long)]
        attempt_cap: Option<u32>,
        /// Daily ledger ceiling across all dispatches
        #[arg(long)]
        daily_budget_usd: Option<f64>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
    /// Review an issue's open PR with a fresh-context Codex reviewer (rung 5)
    Review {
        /// Path to the database
        #[arg(long)]
        db: Option<String>,
        /// Repo identity used as the storage key (e.g. "owner/repo")
        repo: String,
        /// GitHub issue number the PR closes
        issue: u64,
        /// Pull request number to review
        #[arg(long)]
        pr: u64,
        /// The dispatch-time risk class the Lead assigned, compared against
        /// the class the reviewer derives from the diff
        #[arg(long = "class")]
        dispatch_class: String,
        /// Checkout the reviewer reads (read-only to it)
        #[arg(long, default_value = ".")]
        path: String,
        /// Base branch the PR merges into
        #[arg(long, default_value = "main")]
        base: String,
        /// Head branch the IC pushed. Defaults to this issue's autopilot branch.
        #[arg(long)]
        head: Option<String>,
        /// Model for the reviewer; defaults to Codex's own configured model
        #[arg(long)]
        model: Option<String>,
        /// Assert the repo's gate was green when the PR was opened.
        ///
        /// Off by default, and deliberately an *opt-in assertion* rather than
        /// a `--gate-red` opt-out: auto-merge requires green **and** a
        /// reviewer PASS, so a caller who forgets the flag gets a hold, not a
        /// merge. The failure mode of the inverse spelling is shipping
        /// unreviewed code on a red gate.
        #[arg(long)]
        gate_green: bool,
        /// Daily ledger ceiling.
        ///
        /// Note this cannot bound a Codex reviewer on its own: Codex reports
        /// no price, so a review never moves the ledger's dollar total. Use
        /// --max-unpriced-reviews-per-day for that.
        #[arg(long)]
        daily_budget_usd: Option<f64>,
        /// How many unpriced reviewer invocations may run today. The only
        /// bound on reviewer spend that actually holds today.
        #[arg(long)]
        max_unpriced_reviews_per_day: Option<u32>,
        /// Emit JSON instead of text
        #[arg(long)]
        json: bool,
    },
}

/// Where per-issue worktrees live when `--worktree-root` is not given.
///
/// Deliberately *outside* any target repo: a worktree nested inside its own
/// repo shows up in that repo's `git status` and in every glob a build script
/// runs, so Autopilot's checkouts sit beside the database instead, under the
/// same `~/.ironrace-memory` base directory `Config` already owns.
fn default_worktree_root() -> Result<std::path::PathBuf, MemoryError> {
    let home = dirs::home_dir()
        .ok_or_else(|| MemoryError::Config("Cannot determine home directory".into()))?;
    Ok(home.join(".ironrace-memory").join("autopilot-worktrees"))
}

/// Load config, open, and migrate the database — the sequence every
/// `Autopilot` subcommand handler needs before touching storage. Several
/// other command handlers in this file repeat the same three-line sequence
/// inline rather than through a shared helper; that pre-existing duplication
/// is out of scope here, but the new `Autopilot` arms at least don't add
/// to it.
fn open_migrated_db(db: Option<String>) -> Result<ironmem::db::schema::Database, MemoryError> {
    let cfg = config::Config::load(db)?;
    let database = ironmem::db::schema::Database::open(&cfg.db_path)?;
    database.migrate()?;
    Ok(database)
}

/// Subcommands nested under `ironmem symbols`.
#[derive(Subcommand)]
enum SymbolsCmd {
    /// Index a git repository (Rust + Python source files)
    Index {
        /// Path to the git repository root
        #[arg(long)]
        repo: String,
        /// Re-index every file even if content hash is unchanged
        #[arg(long)]
        force: bool,
        /// Emit JSON instead of prose summary
        #[arg(long)]
        json: bool,
    },
    /// Look up symbol declarations by name
    Lookup {
        /// Path to the git repository root
        #[arg(long)]
        repo: String,
        /// Name or qualified-name prefix to search for
        query: String,
        /// Filter by kind (fn, struct, enum, class, …)
        #[arg(long)]
        kind: Option<String>,
        /// Maximum number of results (capped at 200)
        #[arg(long)]
        limit: Option<usize>,
        /// Emit JSON instead of prose
        #[arg(long)]
        json: bool,
    },
    /// Look up import statements by file path or module name
    Imports {
        /// Path to the git repository root
        #[arg(long)]
        repo: String,
        /// File path (repo-relative) or module name prefix
        query: String,
        /// Maximum number of results (capped at 200)
        #[arg(long)]
        limit: Option<usize>,
        /// Emit JSON instead of prose
        #[arg(long)]
        json: bool,
    },
    /// Look up symbol-graph edges (neighbors) by symbol id or file path
    Neighbors {
        /// Path to the git repository root
        #[arg(long)]
        repo: String,
        /// Symbol id or file path prefix
        query: String,
        /// Maximum number of results (capped at 200)
        #[arg(long)]
        limit: Option<usize>,
        /// Emit JSON instead of prose
        #[arg(long)]
        json: bool,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env().add_directive(
                "ironmem=info"
                    .parse()
                    .expect("static directive literal is always valid"),
            ),
        )
        .init();

    let cli = Cli::parse();

    if let Err(e) = run(cli).await {
        eprintln!("Error: {e}");
        process::exit(1);
    }
}

async fn run(cli: Cli) -> Result<(), MemoryError> {
    match cli.command {
        Commands::Serve {
            db,
            listen,
            connect,
            no_autospawn,
        } => {
            let cfg = config::Config::load(db)?;

            // `--listen <socket>`: run as the shared daemon (Unix only, Task 6).
            // `run_daemon` owns its own runtime, so it must not nest inside this
            // `#[tokio::main]` runtime — run it on a dedicated std thread and
            // join. On the None path `cfg` is untouched (the taken branch
            // diverges via `return`), so the stdio fallback below still owns it.
            #[cfg(unix)]
            if let Some(sock) = listen {
                let socket_path = std::path::PathBuf::from(sock);
                return std::thread::spawn(move || mcp::daemon::run_daemon(cfg, socket_path))
                    .join()
                    .map_err(|_| MemoryError::Config("daemon thread panicked".into()))?;
            }
            // On non-unix, `--listen` has no daemon transport yet (Task 10 adds a
            // fallback); consume the flag so the in-process stdio server runs.
            #[cfg(not(unix))]
            let _ = listen;

            // `--connect <socket>`: run as a thin proxy (Unix only, Task 8).
            // `no_autospawn` (CLI flag) forces auto-spawn off regardless of the
            // `IRONMEM_NO_DAEMON` env var; otherwise the env-derived Config
            // setting decides. A successful proxy session returns straight
            // from here; `FallbackToInProcess` (no daemon + autospawn disabled)
            // falls through to the same in-process stdio server used by bare
            // `serve`, below.
            #[cfg(unix)]
            if let Some(sock) = connect {
                let socket_path = std::path::PathBuf::from(sock);
                let autospawn_enabled = !no_autospawn && cfg.daemon_autospawn_enabled();
                // M3: forward this proxy's own resolved db_path so an
                // auto-spawned daemon serves the SAME database, not the
                // default. H5: redirect an auto-spawned daemon's stderr to
                // `<state_dir>/daemon.log` instead of discarding it.
                let daemon_log_path = cfg.state_dir.join("daemon.log");
                match mcp::daemon::run_connect_mode(
                    &socket_path,
                    autospawn_enabled,
                    &cfg.db_path,
                    &daemon_log_path,
                )
                .await?
                {
                    mcp::daemon::ProxyOutcome::Proxied => return Ok(()),
                    mcp::daemon::ProxyOutcome::FallbackToInProcess => {}
                }
            }
            // On non-unix, `--connect` has no proxy transport yet (Task 10 adds
            // a fallback); consume the flags so the in-process stdio server runs.
            #[cfg(not(unix))]
            let _ = (connect, no_autospawn);

            // Phase 1: fast server-ready init (DB open + schema migrate, ~50ms).
            // App is not Sync (single-threaded stdio server, block_in_place dispatch).
            #[allow(clippy::arc_with_non_send_sync)]
            let app = std::sync::Arc::new(mcp::app::App::new_server_ready(cfg.clone())?);
            // Phase 2: model load + bootstrap run in a background thread with its own
            // DB connection (SQLite WAL handles concurrent access safely).
            bootstrap::check_and_record_version(&cfg.state_dir);
            let memory_ready = std::sync::Arc::clone(&app.memory_ready);
            bootstrap::run_background_memory_init(cfg, memory_ready);
            // MCP stdio loop starts immediately — initialize responds in <100ms.
            mcp::server::run_server(app).await
        }
        Commands::Init => {
            let cfg = config::Config::load(None)?;
            cfg.ensure_dirs()?;
            eprintln!("Memory store initialized at {}", cfg.db_path.display());
            Ok(())
        }
        Commands::Setup => {
            let cfg = config::Config::load(None)?;
            if matches!(cfg.embed_mode, config::EmbedMode::Noop) {
                eprintln!("Noop embedder mode enabled; skipping model setup.");
                return Ok(());
            }
            let allow_download = !cfg.model_dir_explicit;
            if allow_download {
                eprintln!(
                    "Preparing embedding model in {}...",
                    cfg.model_dir.display()
                );
            } else {
                eprintln!(
                    "Verifying embedding model in {}...",
                    cfg.model_dir.display()
                );
            }
            ironrace_embed::embedder::ensure_model_in_dir(&cfg.model_dir, allow_download)?;
            eprintln!("Model ready at {}.", cfg.model_dir.display());
            Ok(())
        }
        Commands::Mine { path } => {
            let cfg = config::Config::load(None)?;
            let app = mcp::app::App::new(cfg)?;
            ingest::mine_directory(&app, &path)?;
            Ok(())
        }
        Commands::Migrate { from } => {
            let cfg = config::Config::load(None)?;
            let app = mcp::app::App::new(cfg)?;
            migrate::chromadb::migrate_from_chromadb(&from, &app)?;
            Ok(())
        }
        Commands::Reembed { wing } => {
            let cfg = config::Config::load(None)?;
            let app = mcp::app::App::new(cfg)?;
            reembed::reembed_all(&app, wing.as_deref())?;
            Ok(())
        }
        Commands::Hook { name, harness } => {
            let cfg = config::Config::load(None)?;
            let response = ironmem::hook::run_hook(&name, &harness, cfg)?;
            println!("{}", serde_json::to_string_pretty(&response)?);
            Ok(())
        }
        Commands::Report {
            db,
            task,
            since,
            json,
        } => {
            let cfg = config::Config::load(db)?;
            let database = ironmem::db::schema::Database::open(&cfg.db_path)?;
            database.migrate()?;
            let opts = report::ReportOptions { task, since };
            let report = report::run_report(&database, &opts)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                println!("{}", report::render_text(&report));
            }
            Ok(())
        }
        Commands::Doctor { db, json } => {
            let cfg = config::Config::load(db)?;
            // #190 Task 14: extends run_doctor with the shared-daemon health
            // probe + auto-spawn config, which need an async socket connect.
            let report = ironmem::doctor::run_doctor_with_daemon(&cfg).await;
            if json {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                print!("{}", ironmem::doctor::render_text(&report));
            }
            // Diagnose-only: a clean run exits 0; blocking setup failures exit
            // non-zero so scripts and CI can gate on `ironmem doctor`.
            if report.has_blocking() {
                process::exit(2);
            }
            Ok(())
        }
        Commands::Context {
            db,
            repo,
            task,
            areas,
            budget,
            json,
        } => {
            let cfg = config::Config::load(db)?;
            let app = mcp::app::App::new(cfg)?;
            let opts = context::ContextPackOptions {
                repo: std::path::PathBuf::from(repo),
                task,
                areas,
                budget_tokens: budget,
            };
            let pack = context::run_context(&app, &opts)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&pack)?);
            } else {
                print!("{}", context::render::render_text(&pack));
            }
            Ok(())
        }
        Commands::Claude {
            path,
            prompt,
            no_mcp_setup,
            areas,
            no_context,
            budget,
        } => launcher::run_launcher(
            launcher::Harness::Claude,
            &path,
            prompt,
            launcher::LaunchOptions {
                no_mcp_setup,
                no_context,
                areas,
                budget_tokens: budget,
            },
        ),
        Commands::Codex {
            path,
            prompt,
            no_mcp_setup,
            areas,
            no_context,
            budget,
        } => launcher::run_launcher(
            launcher::Harness::Codex,
            &path,
            prompt,
            launcher::LaunchOptions {
                no_mcp_setup,
                no_context,
                areas,
                budget_tokens: budget,
            },
        ),
        Commands::Grok {
            path,
            prompt,
            no_mcp_setup,
            areas,
            no_context,
            budget,
        } => launcher::run_launcher(
            launcher::Harness::Grok,
            &path,
            prompt,
            launcher::LaunchOptions {
                no_mcp_setup,
                no_context,
                areas,
                budget_tokens: budget,
            },
        ),
        Commands::Gemini {
            path,
            prompt,
            no_mcp_setup,
            areas,
            no_context,
            budget,
        } => launcher::run_launcher(
            launcher::Harness::Gemini,
            &path,
            prompt,
            launcher::LaunchOptions {
                no_mcp_setup,
                no_context,
                areas,
                budget_tokens: budget,
            },
        ),
        Commands::Dashboard {
            db,
            host,
            port,
            allow_non_loopback,
            json,
            exit_on_stdin_close,
        } => {
            let cfg = config::Config::load(db)?;
            let host_addr: std::net::IpAddr = host.parse().map_err(|e| {
                MemoryError::Validation(format!("invalid host address {host:?}: {e}"))
            })?;
            let dash_cfg = dashboard::DashboardConfig {
                db_path: cfg.db_path.clone(),
                host: host_addr,
                port,
                allow_non_loopback,
                json_startup: json,
                exit_on_stdin_close,
                // Reuse the same model-dir resolution as the rest of the CLI so
                // warming status reflects the actual embed cache the binary uses.
                model_dir: cfg.model_dir.clone(),
            };
            dashboard::run_dashboard(dash_cfg).await
        }
        Commands::ReviewDiff {
            repo,
            worktree,
            base,
            head,
            expand_file,
            hunk,
        } => {
            let request = match (worktree, base, head) {
                (true, None, None) => ReviewDiffRequest::worktree(repo),
                (false, Some(base), Some(head)) => ReviewDiffRequest::range(repo, base, head),
                _ => {
                    return Err(MemoryError::Validation(
                        "review-diff source requires exactly --worktree or both --base <rev> --head <rev>"
                            .into(),
                    ));
                }
            };
            if hunk == Some(0) {
                return Err(MemoryError::Validation(
                    "review-diff hunk ordinal must be one-based".into(),
                ));
            }
            match expand_file {
                Some(path) => print!("{}", expand_review_diff(&request, &path, hunk)?),
                None => print!("{}", build_review_diff(&request)?.rendered),
            }
            Ok(())
        }
        Commands::Symbols { cmd } => {
            let cfg = config::Config::load(None)?;
            let db = ironmem::db::schema::Database::open(&cfg.db_path)?;
            db.migrate()?;
            match cmd {
                SymbolsCmd::Index { repo, force, json } => {
                    let result = symbol_graph::index_repo(&db, &repo, force)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&result)?);
                    } else {
                        eprintln!(
                            "indexed: {} files, {} symbols, {} imports, {} edges ({} skipped, {} purged)",
                            result.files_indexed,
                            result.symbols_inserted,
                            result.imports_inserted,
                            result.edges_inserted,
                            result.files_skipped,
                            result.files_purged,
                        );
                    }
                }
                SymbolsCmd::Lookup {
                    repo,
                    query,
                    kind,
                    limit,
                    json,
                } => {
                    let canonical = symbol_graph::canonicalize_repo(&repo)?;
                    let results = symbol_graph::lookup_symbols(
                        &db,
                        &canonical,
                        &query,
                        kind.as_deref(),
                        limit,
                    )?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&results)?);
                    } else {
                        for s in &results {
                            println!(
                                "{} {} {}:{} ({})",
                                s.kind, s.qualified_name, s.path, s.start_line, s.language,
                            );
                        }
                        eprintln!("{} result(s)", results.len());
                    }
                }
                SymbolsCmd::Imports {
                    repo,
                    query,
                    limit,
                    json,
                } => {
                    let canonical = symbol_graph::canonicalize_repo(&repo)?;
                    let results = symbol_graph::lookup_imports(&db, &canonical, &query, limit)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&results)?);
                    } else {
                        for imp in &results {
                            println!("{} → {} (line {})", imp.path, imp.module, imp.line);
                        }
                        eprintln!("{} result(s)", results.len());
                    }
                }
                SymbolsCmd::Neighbors {
                    repo,
                    query,
                    limit,
                    json,
                } => {
                    let canonical = symbol_graph::canonicalize_repo(&repo)?;
                    let results = symbol_graph::lookup_neighbors(&db, &canonical, &query, limit)?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&results)?);
                    } else {
                        for e in &results {
                            println!("{} --{}--> {}", e.from_id, e.edge_kind, e.to_ref);
                        }
                        eprintln!("{} result(s)", results.len());
                    }
                }
            }
            Ok(())
        }
        Commands::Memory { cmd } => match cmd {
            MemoryCmd::Gc {
                db,
                dry_run: _,
                apply,
                collab_checkpoint_days,
                collab_artifact_days,
                limit,
                json,
            } => {
                let cfg = config::Config::load(db)?;
                let database = ironmem::db::schema::Database::open(&cfg.db_path)?;
                database.migrate()?;
                let report = ironmem::db::retention::run_memory_gc(
                    &database,
                    ironmem::db::retention::MemoryGcOptions {
                        apply,
                        collab_checkpoint_days,
                        collab_artifact_days,
                        limit,
                    },
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&report)?);
                } else {
                    print!(
                        "{}",
                        ironmem::db::retention::render_memory_gc_report(&report)
                    );
                }
                Ok(())
            }
        },
        Commands::Autopilot { cmd } => match cmd {
            AutopilotCmd::Onboard {
                db,
                repo,
                path,
                json,
            } => {
                let database = open_migrated_db(db)?;
                let config = ironmem::autopilot::onboard::onboard_repo(
                    &database,
                    &repo,
                    std::path::Path::new(&path),
                )?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                } else {
                    println!(
                        "Proposed gate config for '{}' (pending approval):",
                        config.repo
                    );
                    for gate_command in config.gate_commands() {
                        println!("  - {gate_command}");
                    }
                    if !config.manifest_warnings.is_empty() {
                        println!("Warnings (review before approving):");
                        for warning in &config.manifest_warnings {
                            println!("  - {warning}");
                        }
                    }
                    println!("Run `ironmem autopilot approve {}` to accept.", config.repo);
                }
                Ok(())
            }
            AutopilotCmd::Approve { db, repo, json } => {
                let database = open_migrated_db(db)?;
                let config =
                    ironmem::autopilot::gate_config::approve_gate_config(&database, &repo)?;
                if json {
                    println!("{}", serde_json::to_string_pretty(&config)?);
                } else {
                    println!("Approved gate config for '{}'.", config.repo);
                }
                Ok(())
            }
            AutopilotCmd::Run {
                db,
                repo,
                issue,
                title,
                body,
                body_file,
                path,
                worktree_root,
                base,
                model,
                dispatch_class,
                n_turns,
                max_turns,
                max_budget_usd,
                attempt_cap,
                daily_budget_usd,
                json,
            } => {
                use ironmem::autopilot::run::RunConfig;

                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let body = match (body, body_file) {
                    (Some(text), _) => text,
                    (None, Some(file)) => std::fs::read_to_string(&file).map_err(|e| {
                        MemoryError::NotFound(format!("cannot read --body-file {file}: {e}"))
                    })?,
                    (None, None) => String::new(),
                };

                let mut config = RunConfig::new(model, dispatch_class);
                if let Some(n) = n_turns {
                    config.n_turns = n;
                    // Keep the documented headroom when only N was supplied,
                    // rather than silently failing validation against the
                    // default `max_turns` that was derived from the default N.
                    config.max_turns =
                        n.saturating_add(ironmem::autopilot::run::DEFAULT_MAX_TURNS_HEADROOM);
                }
                if let Some(max) = max_turns {
                    config.max_turns = max;
                }
                if let Some(budget) = max_budget_usd {
                    config.max_budget_usd = budget;
                }
                if let Some(cap) = attempt_cap {
                    config.attempt_cap = cap;
                }
                if let Some(daily) = daily_budget_usd {
                    config.daily_budget_usd = daily;
                }
                // Validate before provisioning a worktree: a bad config
                // should not leave a checkout behind.
                config.validate()?;

                let database = open_migrated_db(db)?;
                // Refuse an unapproved repo *before* creating a worktree and
                // branch for it — `run_issue` checks this too, but by then
                // the checkout already exists on disk.
                ironmem::autopilot::run::approved_gate_commands(&database, &issue_ref.repo)?;

                let worktree_root = match worktree_root {
                    Some(dir) => std::path::PathBuf::from(dir),
                    None => default_worktree_root()?,
                };
                let worktree = ironmem::autopilot::worktree::ensure_worktree(
                    std::path::Path::new(&path),
                    &worktree_root,
                    &issue_ref,
                    &base,
                )?;
                let mut dispatcher = ironmem::autopilot::run::ClaudeDispatcher::resolve()?;
                let run = ironmem::autopilot::run::run_issue(
                    &database,
                    &issue_ref,
                    &ironmem::autopilot::run::IssueBrief { title, body },
                    &worktree,
                    &config,
                    &mut dispatcher,
                )?;

                if json {
                    println!("{}", serde_json::to_string_pretty(&run)?);
                } else {
                    println!("Issue {}", run.issue.canonical());
                    println!("  worktree: {}", worktree.path.display());
                    if let Some(quarantined) = &worktree.quarantined_from {
                        println!("  quarantined dirty worktree to: {}", quarantined.display());
                    }
                    println!("  dispatches: {}", run.dispatches.len());
                    for (i, dispatch) in run.dispatches.iter().enumerate() {
                        println!(
                            "    {}. {:?} — {} turns, ${:.4}{}",
                            i + 1,
                            dispatch.classification,
                            dispatch.num_turns,
                            dispatch.total_cost_usd,
                            dispatch
                                .attempt_n
                                .map(|n| format!(" (attempt {n})"))
                                .unwrap_or_default()
                        );
                    }
                    println!("  spend this run: ${:.4}", run.total_cost_usd);
                    println!("  cumulative attempts: {}", run.cumulative_attempt_n);
                    println!("  terminal: {:?}", run.terminal);
                }
                Ok(())
            }
            AutopilotCmd::Review {
                db,
                repo,
                issue,
                pr,
                dispatch_class,
                path,
                base,
                head,
                model,
                gate_green,
                daily_budget_usd,
                max_unpriced_reviews_per_day,
                json,
            } => {
                let issue_ref = ironmem::autopilot::IssueRef::new(repo, issue);
                let head_branch =
                    head.unwrap_or_else(|| ironmem::autopilot::worktree::branch_name(&issue_ref));

                let database = open_migrated_db(db)?;
                // Refuse an unapproved repo before spending anything: the
                // reviewer's prompt is built from the *approved* gate
                // commands, so there is nothing coherent to dispatch without
                // them.
                let gate_commands =
                    ironmem::autopilot::run::approved_gate_commands(&database, &issue_ref.repo)?;

                let mut runner = ironmem::autopilot::review::CodexReviewer::resolve(model)?;
                let mut review = ironmem::autopilot::review::review_pr(
                    &database,
                    &mut runner,
                    &ironmem::autopilot::review::ReviewRequest {
                        issue: &issue_ref,
                        pr_number: pr,
                        base_branch: &base,
                        head_branch: &head_branch,
                        dispatch_class: &dispatch_class,
                        gate_commands: &gate_commands,
                        gate_green,
                        repo_dir: std::path::Path::new(&path),
                        daily_budget_usd: daily_budget_usd
                            .unwrap_or(ironmem::autopilot::run::DEFAULT_DAILY_BUDGET_USD),
                        max_unpriced_reviews_per_day: max_unpriced_reviews_per_day.unwrap_or(
                            ironmem::autopilot::review::DEFAULT_MAX_UNPRICED_REVIEWS_PER_DAY,
                        ),
                    },
                )?;

                // `record_review` scrubs the reason on the way into storage
                // because a review reason quotes the diff and can carry
                // anything the diff carried. The *emit* path needs the same
                // guarantee: this string goes to stdout and, under --json,
                // into whatever a Lead logs. Scrubbed here rather than inside
                // `review_pr` so the drawer's `reason_redacted` flag still
                // records that a redaction happened.
                if let Some(reason) = review.outcome.reason.take() {
                    review.outcome.reason = Some(
                        ironmem::autopilot::scrub::scrub_and_bound(
                            &reason,
                            ironmem::autopilot::lineage::MAX_LINEAGE_FIELD_CHARS,
                        )
                        .text,
                    );
                }

                if json {
                    println!("{}", serde_json::to_string_pretty(&review)?);
                } else {
                    println!(
                        "Issue {} PR #{}",
                        review.issue.canonical(),
                        review.pr_number
                    );
                    if let Some(refusal) = &review.refusal {
                        println!("  reviewer: not dispatched ({refusal:?})");
                    }
                    println!("  verdict: {:?}", review.outcome.verdict);
                    println!("  diff risk class: {:?}", review.outcome.risk_class);
                    if let Some(reason) = &review.outcome.reason {
                        println!("  reason: {reason}");
                    }
                    if let Some(usage) = &review.outcome.token_usage {
                        println!(
                            "  tokens: {} in ({} cached), {} out — no price reported by Codex",
                            usage.input_tokens, usage.cached_input_tokens, usage.output_tokens
                        );
                    }
                    println!("  decision: {:?}", review.decision);
                    if let Some(id) = &review.record_drawer_id {
                        println!("  recorded: {id}");
                    }
                }
                Ok(())
            }
        },
        Commands::Harnesses { format } => {
            match format.as_str() {
                "json" => println!(
                    "{}",
                    ironmem::harness::registry_json(ironmem::harness::REGISTRY)?
                ),
                _ => print!(
                    "{}",
                    ironmem::harness::registry_text(ironmem::harness::REGISTRY)
                ),
            }
            Ok(())
        }
        Commands::WriteRules {
            target,
            harness,
            workspace,
        } => {
            use ironmem::write_rules::{
                apply_write_rules_plan, build_write_rules_plan, WriteOutcome,
            };
            let plan = build_write_rules_plan(
                std::path::Path::new(&workspace),
                target.as_deref(),
                harness.as_deref(),
                ironmem::harness::REGISTRY,
            )?;
            let outcomes = apply_write_rules_plan(&plan)?;
            for (path, outcome) in outcomes {
                let label = match outcome {
                    WriteOutcome::Created => "created",
                    WriteOutcome::Updated => "updated",
                    WriteOutcome::Unchanged => "unchanged",
                };
                eprintln!("ironmem write-rules: {label} {}", path.display());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: parse argv and return the `Serve` fields, panicking if the parsed
    /// command is not `Serve` (keeps each test focused on field assertions).
    fn parse_serve(args: &[&str]) -> (Option<String>, Option<String>, Option<String>, bool) {
        let cli = Cli::try_parse_from(args).expect("expected argv to parse");
        match cli.command {
            Commands::Serve {
                db,
                listen,
                connect,
                no_autospawn,
            } => (db, listen, connect, no_autospawn),
            _ => panic!("expected Commands::Serve, got a different variant"),
        }
    }

    #[test]
    fn serve_bare_uses_all_defaults() {
        let (db, listen, connect, no_autospawn) = parse_serve(&["ironmem", "serve"]);
        assert_eq!(db, None);
        assert_eq!(listen, None);
        assert_eq!(connect, None);
        assert!(!no_autospawn);
    }

    #[test]
    fn serve_db_preserved_with_new_fields_defaulted() {
        let (db, listen, connect, no_autospawn) =
            parse_serve(&["ironmem", "serve", "--db", "/tmp/x.sqlite3"]);
        assert_eq!(db.as_deref(), Some("/tmp/x.sqlite3"));
        assert_eq!(listen, None);
        assert_eq!(connect, None);
        assert!(!no_autospawn);
    }

    #[test]
    fn serve_listen_sets_listen_only() {
        let (db, listen, connect, no_autospawn) =
            parse_serve(&["ironmem", "serve", "--listen", "/tmp/d.sock"]);
        assert_eq!(db, None);
        assert_eq!(listen.as_deref(), Some("/tmp/d.sock"));
        assert_eq!(connect, None);
        assert!(!no_autospawn);
    }

    #[test]
    fn serve_connect_sets_connect_only() {
        let (db, listen, connect, no_autospawn) =
            parse_serve(&["ironmem", "serve", "--connect", "/tmp/d.sock"]);
        assert_eq!(db, None);
        assert_eq!(listen, None);
        assert_eq!(connect.as_deref(), Some("/tmp/d.sock"));
        assert!(!no_autospawn);
    }

    #[test]
    fn serve_listen_and_connect_are_mutually_exclusive() {
        let result = Cli::try_parse_from([
            "ironmem",
            "serve",
            "--listen",
            "/a.sock",
            "--connect",
            "/b.sock",
        ]);
        assert!(result.is_err(), "expected --listen + --connect to conflict");
        let msg = result.err().unwrap().to_string();
        assert!(
            msg.contains("connect") && msg.contains("listen"),
            "conflict error should mention both flags, got: {msg}"
        );
    }

    #[test]
    fn serve_no_autospawn_with_connect() {
        let (_db, listen, connect, no_autospawn) = parse_serve(&[
            "ironmem",
            "serve",
            "--no-autospawn",
            "--connect",
            "/tmp/d.sock",
        ]);
        assert_eq!(listen, None);
        assert_eq!(connect.as_deref(), Some("/tmp/d.sock"));
        assert!(no_autospawn);
    }

    #[test]
    fn autopilot_onboard_defaults_path_to_current_directory() {
        let cli = Cli::try_parse_from(["ironmem", "autopilot", "onboard", "owner/repo"])
            .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Onboard {
                        repo,
                        path,
                        db,
                        json,
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(path, ".");
                assert_eq!(db, None);
                assert!(!json);
            }
            _ => panic!("expected Commands::Autopilot(Onboard), got a different variant"),
        }
    }

    #[test]
    fn autopilot_onboard_accepts_an_explicit_path() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "onboard",
            "owner/repo",
            "--path",
            "/tmp/some-checkout",
        ])
        .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd: AutopilotCmd::Onboard { path, .. },
            } => assert_eq!(path, "/tmp/some-checkout"),
            _ => panic!("expected Commands::Autopilot(Onboard), got a different variant"),
        }
    }

    #[test]
    fn autopilot_run_parses_the_issue_and_brief() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "run",
            "owner/repo",
            "283",
            "--title",
            "Make the gate pass",
            "--body",
            "The suite is red.",
        ])
        .expect("parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Run {
                        repo,
                        issue,
                        title,
                        body,
                        base,
                        dispatch_class,
                        ..
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(issue, 283);
                assert_eq!(title, "Make the gate pass");
                assert_eq!(body.as_deref(), Some("The suite is red."));
                assert_eq!(base, "HEAD");
                assert_eq!(dispatch_class, "unclassified");
            }
            _ => panic!("expected Commands::Autopilot(Run), got a different variant"),
        }
    }

    #[test]
    fn autopilot_review_parses_the_pr_and_class() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "review",
            "owner/repo",
            "283",
            "--pr",
            "322",
            "--class",
            "documentation",
        ])
        .expect("review args should parse");
        match cli.command {
            Commands::Autopilot {
                cmd:
                    AutopilotCmd::Review {
                        repo,
                        issue,
                        pr,
                        dispatch_class,
                        base,
                        head,
                        gate_green,
                        ..
                    },
            } => {
                assert_eq!(repo, "owner/repo");
                assert_eq!(issue, 283);
                assert_eq!(pr, 322);
                assert_eq!(dispatch_class, "documentation");
                assert_eq!(base, "main");
                assert_eq!(head, None, "head defaults to the issue's autopilot branch");
                assert!(
                    !gate_green,
                    "the gate must not be assumed green when the flag is absent"
                );
            }
            _ => panic!("expected Commands::Autopilot(Review), got a different variant"),
        }
    }

    #[test]
    fn autopilot_review_requires_a_pr_and_a_dispatch_class() {
        // Without `--pr` there is nothing to review; without `--class` the
        // double-classification check has nothing to compare against, and
        // defaulting it would make one half of the spec's fail-closed rule
        // silently vacuous.
        assert!(Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "review",
            "owner/repo",
            "283",
            "--class",
            "documentation",
        ])
        .is_err());
        assert!(Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "review",
            "owner/repo",
            "283",
            "--pr",
            "322",
        ])
        .is_err());
    }

    #[test]
    fn autopilot_review_gate_green_is_opt_in() {
        let cli = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "review",
            "owner/repo",
            "283",
            "--pr",
            "322",
            "--class",
            "test_only",
            "--gate-green",
        ])
        .expect("review args should parse");
        match cli.command {
            Commands::Autopilot {
                cmd: AutopilotCmd::Review { gate_green, .. },
            } => assert!(gate_green),
            _ => panic!("expected Commands::Autopilot(Review), got a different variant"),
        }
    }

    #[test]
    fn autopilot_run_requires_a_title() {
        let result = Cli::try_parse_from(["ironmem", "autopilot", "run", "owner/repo", "283"]);
        assert!(result.is_err(), "--title is required for the turn prompt");
    }

    #[test]
    fn autopilot_run_rejects_both_body_and_body_file() {
        let result = Cli::try_parse_from([
            "ironmem",
            "autopilot",
            "run",
            "owner/repo",
            "283",
            "--title",
            "t",
            "--body",
            "inline",
            "--body-file",
            "issue.md",
        ]);
        assert!(
            result.is_err(),
            "--body and --body-file are mutually exclusive"
        );
    }

    #[test]
    fn autopilot_approve_requires_a_repo_argument() {
        let result = Cli::try_parse_from(["ironmem", "autopilot", "approve"]);
        assert!(
            result.is_err(),
            "expected missing repo argument to fail parsing"
        );
    }

    #[test]
    fn autopilot_approve_parses_repo_and_json_flag() {
        let cli = Cli::try_parse_from(["ironmem", "autopilot", "approve", "owner/repo", "--json"])
            .expect("expected argv to parse");
        match cli.command {
            Commands::Autopilot {
                cmd: AutopilotCmd::Approve { repo, json, .. },
            } => {
                assert_eq!(repo, "owner/repo");
                assert!(json);
            }
            _ => panic!("expected Commands::Autopilot(Approve), got a different variant"),
        }
    }
}
