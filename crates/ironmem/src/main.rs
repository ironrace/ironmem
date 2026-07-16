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
                match mcp::daemon::run_connect_mode(&socket_path, autospawn_enabled).await? {
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
}
