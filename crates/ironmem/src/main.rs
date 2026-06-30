use clap::{Parser, Subcommand};
use std::process;

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
        /// Validated at runtime against registered harness rules files.
        #[arg(long, conflicts_with = "harness")]
        target: Option<String>,
        /// Write only this harness's rules file (e.g. claude → CLAUDE.md, codex → AGENTS.md).
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
    },
    /// Build or query the local symbol/import graph index
    Symbols {
        #[command(subcommand)]
        cmd: SymbolsCmd,
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
        Commands::Serve { db } => {
            let cfg = config::Config::load(db)?;
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
            let report = ironmem::doctor::run_doctor(&cfg);
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
        Commands::Dashboard {
            db,
            host,
            port,
            allow_non_loopback,
            json,
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
                // Reuse the same model-dir resolution as the rest of the CLI so
                // warming status reflects the actual embed cache the binary uses.
                model_dir: cfg.model_dir.clone(),
            };
            dashboard::run_dashboard(dash_cfg).await
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
                resolve_write_targets, validate_rules_file, write_rules_file, WriteOutcome,
            };
            let targets = resolve_write_targets(
                target.as_deref(),
                harness.as_deref(),
                ironmem::harness::REGISTRY,
            )?;
            let paths: Vec<_> = targets
                .iter()
                .map(|name| std::path::Path::new(&workspace).join(name))
                .collect();
            // For the default multi-file run, pre-validate every target so a
            // malformed managed block in one file aborts before any file is
            // written. This makes *validation* all-or-nothing; the writes
            // themselves are still applied sequentially (a write-time I/O error
            // on the second file leaves the first written). Single-target runs
            // need no preflight — there is nothing to roll back.
            if targets.len() > 1 {
                for path in &paths {
                    validate_rules_file(path, bootstrap::MEMORY_PROTOCOL)?;
                }
            }
            for path in paths {
                let outcome = write_rules_file(&path, bootstrap::MEMORY_PROTOCOL)?;
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
