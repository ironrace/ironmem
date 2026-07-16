//! One-command launchers: validate the assistant binary, canonicalize and warm
//! the target repo, ensure the ironmem MCP server is registered, then launch
//! the assistant with the repo as its working directory.

mod argv;
mod binary;
mod context_inject;
mod mcp_setup;

use std::path::{Path, PathBuf};

use crate::error::MemoryError;
use crate::{config, ingest, mcp};

/// The assistant a launcher targets.
///
/// Each variant's behavior (`spec`/`binary`/`label`) is entirely
/// registry-driven (#190 Task 13): `harness_id` is the only per-variant
/// mapping, and everything else comes from `crate::harness::REGISTRY` via
/// `spec()`. `Grok`/`Gemini` onboard the same way Claude/Codex already did —
/// one registry row (added in Task 11) plus one `harness_id` mapping here.
/// The enum itself (and clap's `Commands::Grok`/`Commands::Gemini` in
/// `main.rs`) still needs one variant per launchable harness: clap subcommands
/// are compile-time, so the CLI surface can't be generated from a runtime
/// registry without a larger redesign (e.g. a single generic `launch
/// --harness <id>` subcommand) — out of scope here.
#[derive(Debug, Clone, Copy)]
pub enum Harness {
    Claude,
    Codex,
    Grok,
    Gemini,
}

impl Harness {
    /// Canonical registry id for this variant.  This is the single link from
    /// the launcher's protocol-role enum to the harness registry.
    pub fn harness_id(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
            Harness::Grok => "grok",
            Harness::Gemini => "gemini",
        }
    }

    /// Look up the full registry spec for this harness.
    ///
    /// # Panics
    ///
    /// Panics if `REGISTRY` does not contain an entry for `self.harness_id()`.
    /// This is a programming error: every `Harness` variant must have a
    /// corresponding entry in `crate::harness::REGISTRY`.
    pub fn spec(self) -> &'static crate::harness::HarnessSpec {
        crate::harness::by_id(self.harness_id(), crate::harness::REGISTRY)
            .expect("REGISTRY must contain the launcher harness id")
    }

    /// Executable name looked up on PATH.  Derived from the registry spec.
    pub fn binary(self) -> &'static str {
        self.spec().binary
    }

    /// Human-readable name for log lines.  Derived from the registry spec.
    pub fn label(self) -> &'static str {
        self.spec().display_name
    }
}

/// Options for a launcher invocation. Grouped into a struct so the public
/// `run_launcher` seam stays readable as flags accrue (issue #147).
#[derive(Debug, Clone)]
pub struct LaunchOptions {
    /// Skip idempotent ironmem MCP server registration.
    pub no_mcp_setup: bool,
    /// Disable compact context pre-injection into the initial prompt.
    pub no_context: bool,
    /// Code-map areas to include in the pre-injected context (repeatable).
    pub areas: Vec<String>,
    /// Approximate token budget for the pre-injected context pack.
    pub budget_tokens: usize,
}

/// Resolve a user-supplied repo path to an existing canonical directory.
fn canonicalize_repo(repo: &str) -> Result<PathBuf, MemoryError> {
    let resolved = std::fs::canonicalize(repo)
        .map_err(|e| MemoryError::NotFound(format!("repo path not found: {repo}: {e}")))?;
    if !resolved.is_dir() {
        return Err(MemoryError::Validation(format!(
            "repo path is not a directory: {}",
            resolved.display()
        )));
    }
    Ok(resolved)
}

/// Resolve the Codex config path: `$CODEX_HOME/config.toml`, else
/// `~/.codex/config.toml`. Mirrors `doctor::codex_config_path`.
fn codex_config_path() -> Result<PathBuf, MemoryError> {
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home).join("config.toml"));
        }
    }
    let home = dirs::home_dir()
        .ok_or_else(|| MemoryError::Config("cannot determine home directory".into()))?;
    Ok(home.join(".codex").join("config.toml"))
}

/// Resolve the Claude config path: `~/.claude.json`. Mirrors `doctor::detect_claude`.
fn claude_config_path() -> Result<PathBuf, MemoryError> {
    let home = dirs::home_dir()
        .ok_or_else(|| MemoryError::Config("cannot determine home directory".into()))?;
    Ok(home.join(".claude.json"))
}

/// Resolve the Gemini CLI config path: `~/.gemini/settings.json`. This is
/// Gemini CLI's documented global settings file, with a top-level
/// `mcpServers` object in the exact same shape Claude's `~/.claude.json` uses
/// — hence `ensure_json_mcpservers_registered` reuses the same writer.
fn gemini_config_path() -> Result<PathBuf, MemoryError> {
    let home = dirs::home_dir()
        .ok_or_else(|| MemoryError::Config("cannot determine home directory".into()))?;
    Ok(home.join(".gemini").join("settings.json"))
}

/// Resolve a best-effort Grok CLI config path: `~/.grok/settings.json`.
///
/// Unlike Claude/Codex/Gemini, there is no single confirmed "the" Grok CLI
/// config convention as of #190 Task 13 — multiple tools answer to "grok
/// cli" (xAI's own "Grok Build" agent vs. community projects), and
/// documentation for either's MCP client config is thin. `.grok/settings.json`
/// with an `mcpServers` key is `superagent-ai/grok-cli`'s documented
/// convention and the best-effort default here; grok registration is
/// deliberately scaffolding (`write_rules_default: false` on its registry
/// row) until a real, confirmed convention narrows this down.
fn grok_config_path() -> Result<PathBuf, MemoryError> {
    let home = dirs::home_dir()
        .ok_or_else(|| MemoryError::Config("cannot determine home directory".into()))?;
    Ok(home.join(".grok").join("settings.json"))
}

/// Ensure the ironmem MCP server is registered for `harness`, idempotently.
///
/// Registers the shared-daemon proxy command (`harness::proxy_command_args` —
/// `["serve", "--connect", <socket>]`), the canonical invocation every
/// harness should converge on (#190 Task 11/12). Claude, Gemini CLI, and
/// (best-effort) Grok CLI all share the same `mcpServers`-JSON config shape,
/// so all three route through `ensure_json_mcpservers_registered`; only Codex
/// uses a different (TOML) format.
fn register(harness: Harness) -> Result<mcp_setup::RegisterOutcome, MemoryError> {
    let exe = std::env::current_exe()
        .map_err(|e| MemoryError::Config(format!("cannot resolve ironmem path: {e}")))?;
    let exe = exe.to_string_lossy().to_string();
    let cfg = config::Config::load(None)?;
    let proxy_args = crate::harness::proxy_command_args(harness.harness_id(), &cfg);
    match harness {
        Harness::Claude => {
            mcp_setup::ensure_json_mcpservers_registered(&claude_config_path()?, &exe, &proxy_args)
        }
        Harness::Codex => {
            mcp_setup::ensure_codex_registered(&codex_config_path()?, &exe, &proxy_args)
        }
        Harness::Gemini => {
            mcp_setup::ensure_json_mcpservers_registered(&gemini_config_path()?, &exe, &proxy_args)
        }
        Harness::Grok => {
            mcp_setup::ensure_json_mcpservers_registered(&grok_config_path()?, &exe, &proxy_args)
        }
    }
}

/// Build the app and mine the repo into memory. Separated so the caller can
/// treat warming as best-effort.
fn warm(canonical: &Path) -> Result<(), MemoryError> {
    let cfg = config::Config::load(None)?;
    let app = mcp::app::App::new(cfg)?;
    ingest::mine_directory(&app, &canonical.to_string_lossy())?;
    Ok(())
}

/// Warm the repo, logging (not failing) on error: the launched assistant's own
/// MCP server bootstraps on `serve`, so a warming hiccup must not block launch.
fn warm_best_effort(canonical: &Path) {
    if let Err(e) = warm(canonical) {
        eprintln!(
            "ironmem: warning: could not warm {} ({e}); the assistant will warm on first use",
            canonical.display()
        );
    }
}

/// Entry point for the `claude` / `codex` subcommands.
pub fn run_launcher(
    harness: Harness,
    repo: &str,
    prompt: Option<String>,
    opts: LaunchOptions,
) -> Result<(), MemoryError> {
    let canonical = canonicalize_repo(repo)?;
    let bin = binary::find_on_path(harness.binary())?;

    if opts.no_mcp_setup {
        eprintln!("ironmem: skipping MCP setup (--no-mcp-setup); using existing configuration");
    } else {
        match register(harness)? {
            mcp_setup::RegisterOutcome::Registered => {
                eprintln!("ironmem: registered MCP server for {}", harness.label())
            }
            mcp_setup::RegisterOutcome::Upgraded => {
                eprintln!(
                    "ironmem: upgraded MCP server registration for {} to the shared-daemon proxy command",
                    harness.label()
                )
            }
            mcp_setup::RegisterOutcome::AlreadyRegistered => {
                eprintln!(
                    "ironmem: MCP server already registered for {}",
                    harness.label()
                )
            }
        }
    }

    warm_best_effort(&canonical);

    // Best-effort context pre-injection AFTER warming, so the just-mined repo and
    // FTS index are available to recall. Falls back to the bare prompt on any
    // failure or when disabled.
    let prompt = context_inject::maybe_inject_context(&canonical, prompt, &opts);

    let args = argv::build_args(prompt.as_deref());
    eprintln!(
        "ironmem: launching {} in {}",
        harness.label(),
        canonical.display()
    );
    let status = std::process::Command::new(&bin)
        .args(&args)
        .current_dir(&canonical)
        .status()
        .map_err(|e| {
            MemoryError::NotFound(format!("failed to launch {}: {e}", harness.binary()))
        })?;

    if !status.success() {
        #[cfg(unix)]
        let code = status.code().unwrap_or_else(|| {
            use std::os::unix::process::ExitStatusExt;
            128 + status.signal().unwrap_or(0)
        });
        #[cfg(not(unix))]
        let code = status.code().unwrap_or(1);
        std::process::exit(code);
    }
    Ok(())
}

/// Build the `(binary, argv)` pair for launching a harness from its spec.
///
/// Pure and registry-driven so a synthetic third harness can be exercised
/// without a real binary on PATH.
#[cfg(test)]
pub(crate) fn launch_invocation(
    spec: &crate::harness::HarnessSpec,
    prompt: Option<&str>,
) -> (String, Vec<String>) {
    (spec.binary.to_string(), argv::build_args(prompt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{HarnessSpec, TranscriptParserKind};

    #[test]
    fn harness_maps_to_binary_and_label() {
        assert_eq!(Harness::Claude.binary(), "claude");
        assert_eq!(Harness::Codex.binary(), "codex");
        assert_eq!(Harness::Claude.label(), "Claude Code");
        assert_eq!(Harness::Codex.label(), "Codex");
    }

    /// #190 Task 13 acceptance: grok/gemini launchers resolve from the
    /// registry (added in Task 11) exactly the way Claude/Codex already do —
    /// one registry row, one `harness_id` mapping, everything else derived.
    #[test]
    fn grok_and_gemini_harness_resolve_from_registry() {
        assert_eq!(Harness::Grok.harness_id(), "grok");
        assert_eq!(Harness::Grok.binary(), "grok");
        assert_eq!(Harness::Grok.label(), "Grok");

        assert_eq!(Harness::Gemini.harness_id(), "gemini");
        assert_eq!(Harness::Gemini.binary(), "gemini");
        assert_eq!(Harness::Gemini.label(), "Gemini CLI");
    }

    #[test]
    fn harness_id_resolves_via_registry() {
        let claude_spec =
            crate::harness::by_id(Harness::Claude.harness_id(), crate::harness::REGISTRY)
                .expect("claude must be in REGISTRY");
        assert_eq!(claude_spec.id, "claude");
        assert_eq!(claude_spec.binary, "claude");
        assert_eq!(claude_spec.display_name, "Claude Code");

        let codex_spec =
            crate::harness::by_id(Harness::Codex.harness_id(), crate::harness::REGISTRY)
                .expect("codex must be in REGISTRY");
        assert_eq!(codex_spec.id, "codex");
        assert_eq!(codex_spec.binary, "codex");
        assert_eq!(codex_spec.display_name, "Codex");

        let grok_spec = crate::harness::by_id(Harness::Grok.harness_id(), crate::harness::REGISTRY)
            .expect("grok must be in REGISTRY");
        assert_eq!(grok_spec.id, "grok");

        let gemini_spec =
            crate::harness::by_id(Harness::Gemini.harness_id(), crate::harness::REGISTRY)
                .expect("gemini must be in REGISTRY");
        assert_eq!(gemini_spec.id, "gemini");
    }

    #[test]
    fn grok_config_path_defaults_to_home_grok_settings() {
        let path = grok_config_path().unwrap();
        assert!(
            path.ends_with(".grok/settings.json"),
            "got: {}",
            path.display()
        );
    }

    #[test]
    fn gemini_config_path_defaults_to_home_gemini_settings() {
        let path = gemini_config_path().unwrap();
        assert!(
            path.ends_with(".gemini/settings.json"),
            "got: {}",
            path.display()
        );
    }

    const GEMINI_SPEC: HarnessSpec = HarnessSpec {
        id: "gemini",
        display_name: "Gemini",
        binary: "gemini",
        rules_file: "GEMINI.md",
        rules_strategy: crate::harness::RulesStrategy::Import {
            directive: "@./AGENTS.md",
        },
        write_rules_default: false,
        client_info_aliases: &["gemini"],
        env_aliases: &["gemini"],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    };

    #[test]
    fn launch_invocation_third_harness_with_prompt() {
        let (bin, args) = launch_invocation(&GEMINI_SPEC, Some("do the thing"));
        assert_eq!(bin, "gemini");
        assert_eq!(args, vec!["do the thing".to_string()]);
    }

    #[test]
    fn launch_invocation_third_harness_no_prompt() {
        let (bin, args) = launch_invocation(&GEMINI_SPEC, None);
        assert_eq!(bin, "gemini");
        assert!(args.is_empty());
    }

    #[test]
    fn canonicalize_accepts_existing_dir() {
        let dir = tempfile::tempdir().unwrap();
        let resolved = canonicalize_repo(&dir.path().to_string_lossy()).unwrap();
        assert!(resolved.is_dir());
        assert_eq!(resolved, std::fs::canonicalize(dir.path()).unwrap());
    }

    #[test]
    fn canonicalize_rejects_missing_path() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        let err = canonicalize_repo(&missing.to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("not found"), "got: {err}");
    }

    #[test]
    fn canonicalize_rejects_a_file() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.txt");
        std::fs::write(&file, "x").unwrap();
        let err = canonicalize_repo(&file.to_string_lossy()).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "got: {err}");
    }
}
