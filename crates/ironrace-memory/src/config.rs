//! Configuration loading and environment-variable parsing for `ironmem`.

use std::path::PathBuf;

use crate::error::MemoryError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpAccessMode {
    Trusted,
    ReadOnly,
    Restricted,
}

impl McpAccessMode {
    fn parse(raw: &str) -> Result<Self, MemoryError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "trusted" => Ok(Self::Trusted),
            "read-only" | "readonly" => Ok(Self::ReadOnly),
            "restricted" => Ok(Self::Restricted),
            other => Err(MemoryError::Config(format!(
                "IRONMEM_MCP_MODE must be one of trusted, read-only, restricted; got {other}"
            ))),
        }
    }

    pub fn allows_writes(self) -> bool {
        matches!(self, Self::Trusted)
    }

    pub fn redacts_sensitive_content(self) -> bool {
        matches!(self, Self::Restricted)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedMode {
    Real,
    Noop,
}

impl EmbedMode {
    fn parse(raw: &str) -> Result<Self, MemoryError> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "real" => Ok(Self::Real),
            "noop" | "no-op" => Ok(Self::Noop),
            other => Err(MemoryError::Config(format!(
                "IRONMEM_EMBED_MODE must be one of real, noop; got {other}"
            ))),
        }
    }
}

/// Application configuration.
///
/// Priority: CLI arg > env var > config file > defaults.
#[derive(Debug, Clone)]
pub struct Config {
    pub db_path: PathBuf,
    pub model_dir: PathBuf,
    pub model_dir_explicit: bool,
    pub state_dir: PathBuf,
    pub mcp_access_mode: McpAccessMode,
    pub embed_mode: EmbedMode,
}

impl Config {
    /// Load configuration, optionally overriding the database path.
    pub fn load(db_override: Option<String>) -> Result<Self, MemoryError> {
        let home = dirs::home_dir()
            .ok_or_else(|| MemoryError::Config("Cannot determine home directory".into()))?;

        let base_dir = home.join(".ironrace-memory");

        let db_path = if let Some(p) = db_override {
            PathBuf::from(p)
        } else if let Ok(p) = std::env::var("IRONMEM_DB_PATH") {
            PathBuf::from(p)
        } else {
            base_dir.join("memory.sqlite3")
        };

        let (model_dir, model_dir_explicit) = if let Ok(p) = std::env::var("IRONMEM_MODEL_DIR") {
            (PathBuf::from(p), true)
        } else {
            // Use the same cache dir as the embed crate so they stay in sync
            // when the model is upgraded.
            let dir = ironrace_embed::embedder::model_cache_dir().unwrap_or_else(|_| {
                home.join(".ironrace")
                    .join("models")
                    .join("all-MiniLM-L6-v2")
            });
            (dir, false)
        };

        let state_dir = base_dir.join("hook_state");
        let mcp_access_mode = match std::env::var("IRONMEM_MCP_MODE") {
            Ok(mode) => McpAccessMode::parse(&mode)?,
            Err(_) => McpAccessMode::ReadOnly,
        };
        let embed_mode = match std::env::var("IRONMEM_EMBED_MODE") {
            Ok(mode) => EmbedMode::parse(&mode)?,
            Err(_) => EmbedMode::Real,
        };

        Ok(Self {
            db_path,
            model_dir,
            model_dir_explicit,
            state_dir,
            mcp_access_mode,
            embed_mode,
        })
    }

    /// Ensure all required directories exist.
    pub fn ensure_dirs(&self) -> Result<(), MemoryError> {
        if let Some(parent) = self.db_path.parent() {
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700));
            }
        }
        std::fs::create_dir_all(&self.state_dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                std::fs::set_permissions(&self.state_dir, std::fs::Permissions::from_mode(0o700));
        }
        Ok(())
    }
}
