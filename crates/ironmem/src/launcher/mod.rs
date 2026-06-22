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
#[derive(Debug, Clone, Copy)]
pub enum Harness {
    Claude,
    Codex,
}

impl Harness {
    /// Executable name looked up on PATH.
    pub fn binary(self) -> &'static str {
        match self {
            Harness::Claude => "claude",
            Harness::Codex => "codex",
        }
    }

    /// Human-readable name for log lines.
    pub fn label(self) -> &'static str {
        match self {
            Harness::Claude => "Claude Code",
            Harness::Codex => "Codex",
        }
    }
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

/// Ensure the ironmem MCP server is registered for `harness`, idempotently.
fn register(harness: Harness) -> Result<mcp_setup::RegisterOutcome, MemoryError> {
    let exe = std::env::current_exe()
        .map_err(|e| MemoryError::Config(format!("cannot resolve ironmem path: {e}")))?;
    let exe = exe.to_string_lossy().to_string();
    match harness {
        Harness::Claude => mcp_setup::ensure_claude_registered(&claude_config_path()?, &exe),
        Harness::Codex => mcp_setup::ensure_codex_registered(&codex_config_path()?, &exe),
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
    no_mcp_setup: bool,
) -> Result<(), MemoryError> {
    let canonical = canonicalize_repo(repo)?;
    let bin = binary::find_on_path(harness.binary())?;

    if no_mcp_setup {
        eprintln!("ironmem: skipping MCP setup (--no-mcp-setup); using existing configuration");
    } else {
        match register(harness)? {
            mcp_setup::RegisterOutcome::Registered => {
                eprintln!("ironmem: registered MCP server for {}", harness.label())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn harness_maps_to_binary_and_label() {
        assert_eq!(Harness::Claude.binary(), "claude");
        assert_eq!(Harness::Codex.binary(), "codex");
        assert_eq!(Harness::Claude.label(), "Claude Code");
        assert_eq!(Harness::Codex.label(), "Codex");
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
