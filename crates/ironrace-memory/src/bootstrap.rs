use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::MemoryError;
use crate::mcp::app::App;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GlobalBootstrapState {
    pub initialized_at: Option<String>,
    pub migration_source: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspaceBootstrapState {
    pub workspace_root: String,
    pub initial_mine_completed: bool,
    pub last_mined_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BootstrapReport {
    pub initialized_store: bool,
    pub migration_source: Option<String>,
    pub initial_mine_ran: bool,
    pub workspace_root: Option<String>,
}

pub const MEMORY_PROTOCOL: &str = "Before answering questions about prior work, decisions, project history, or people, check ironmem_search or the KG tools first. After important progress or decisions, write durable summaries back into memory.";

pub fn auto_bootstrap_enabled() -> bool {
    std::env::var("IRONMEM_AUTO_BOOTSTRAP")
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "0" | "false" | "no"
            )
        })
        .unwrap_or(true)
}

pub fn resolve_workspace_root(explicit: Option<&Path>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path.to_path_buf());
    }
    if let Ok(path) = std::env::var("IRONMEM_WORKSPACE_ROOT") {
        let trimmed = path.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed));
        }
    }
    None
}

pub fn ensure_bootstrapped(
    app: &App,
    workspace_root: Option<&Path>,
) -> Result<BootstrapReport, MemoryError> {
    if !auto_bootstrap_enabled() {
        return Ok(BootstrapReport::default());
    }

    let _lock = BootstrapLock::acquire(&app.config.state_dir)?;

    let mut report = BootstrapReport::default();
    let global_state_path = global_state_path(&app.config);
    let mut global_state = load_global_state(&global_state_path)?;

    if app.db.count_drawers(None)? == 0 {
        if let Some(source) = detect_mempalace_store() {
            crate::migrate::chromadb::migrate_from_chromadb(
                source.to_string_lossy().as_ref(),
                app,
            )?;
            report.migration_source = Some(source.display().to_string());
            global_state.migration_source = report.migration_source.clone();
        }
        report.initialized_store = true;
        if global_state.initialized_at.is_none() {
            global_state.initialized_at = Some(chrono::Utc::now().to_rfc3339());
        }
        save_json(&global_state_path, &global_state)?;
    }

    if let Some(workspace) = resolve_workspace_root(workspace_root) {
        let workspace_state_path = workspace_state_path(&app.config, &workspace);
        let mut workspace_state = load_workspace_state(&workspace_state_path, &workspace)?;
        if !workspace_state.initial_mine_completed {
            crate::ingest::mine_directory(app, workspace.to_string_lossy().as_ref())?;
            workspace_state.initial_mine_completed = true;
            workspace_state.last_mined_at = Some(chrono::Utc::now().to_rfc3339());
            save_json(&workspace_state_path, &workspace_state)?;
            report.initial_mine_ran = true;
        }
        report.workspace_root = Some(workspace.display().to_string());
    }

    Ok(report)
}

struct BootstrapLock {
    path: PathBuf,
}

impl BootstrapLock {
    fn acquire(state_dir: &Path) -> Result<Self, MemoryError> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("bootstrap.lock");
        let start = Instant::now();
        loop {
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if start.elapsed() > Duration::from_secs(10) {
                        return Err(MemoryError::Io(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            format!("Timed out waiting for bootstrap lock at {}", path.display()),
                        )));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(error) => return Err(MemoryError::Io(error)),
            }
        }
    }
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn record_workspace_mine(config: &Config, workspace_root: &Path) -> Result<(), MemoryError> {
    let workspace_state_path = workspace_state_path(config, workspace_root);
    let mut workspace_state = load_workspace_state(&workspace_state_path, workspace_root)?;
    workspace_state.initial_mine_completed = true;
    workspace_state.last_mined_at = Some(chrono::Utc::now().to_rfc3339());
    save_json(&workspace_state_path, &workspace_state)
}

pub fn detect_mempalace_store() -> Option<PathBuf> {
    if std::env::var("IRONMEM_DISABLE_MIGRATION")
        .map(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes"
            )
        })
        .unwrap_or(false)
    {
        return None;
    }

    if let Ok(path) = std::env::var("IRONMEM_MIGRATE_FROM") {
        let candidate = PathBuf::from(path);
        if candidate.join("chroma.sqlite3").is_file() {
            return Some(candidate);
        }
    }

    if let Ok(path) = std::env::var("MEMPALACE_PALACE_PATH") {
        let candidate = PathBuf::from(path);
        if candidate.join("chroma.sqlite3").is_file() {
            return Some(candidate);
        }
    }

    if let Ok(path) = std::env::var("MEMPAL_PALACE_PATH") {
        let candidate = PathBuf::from(path);
        if candidate.join("chroma.sqlite3").is_file() {
            return Some(candidate);
        }
    }

    if let Some(home) = dirs::home_dir() {
        let default = home.join(".mempalace").join("palace");
        if default.join("chroma.sqlite3").is_file() {
            return Some(default);
        }

        let config_path = home.join(".mempalace").join("config.json");
        if let Ok(raw) = std::fs::read_to_string(config_path) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) {
                if let Some(path) = json.get("palace_path").and_then(|value| value.as_str()) {
                    let candidate = PathBuf::from(path);
                    if candidate.join("chroma.sqlite3").is_file() {
                        return Some(candidate);
                    }
                }
            }
        }
    }

    None
}

fn global_state_path(config: &Config) -> PathBuf {
    config.state_dir.join("bootstrap.json")
}

fn workspace_state_path(config: &Config, workspace_root: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(workspace_root.to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    config
        .state_dir
        .join("workspaces")
        .join(format!("{}.json", &key[..16]))
}

fn load_global_state(path: &Path) -> Result<GlobalBootstrapState, MemoryError> {
    load_json(path)
}

fn load_workspace_state(
    path: &Path,
    workspace_root: &Path,
) -> Result<WorkspaceBootstrapState, MemoryError> {
    let mut state: WorkspaceBootstrapState = load_json(path)?;
    if state.workspace_root.is_empty() {
        state.workspace_root = workspace_root.display().to_string();
    }
    Ok(state)
}

fn load_json<T>(path: &Path) -> Result<T, MemoryError>
where
    T: Default + for<'de> Deserialize<'de>,
{
    if !path.is_file() {
        return Ok(T::default());
    }
    let raw = std::fs::read_to_string(path)?;
    match serde_json::from_str(&raw) {
        Ok(value) => Ok(value),
        Err(error) => {
            tracing::warn!(
                "Ignoring malformed bootstrap state at {}: {}",
                path.display(),
                error
            );
            Ok(T::default())
        }
    }
}

fn save_json<T: Serialize>(path: &Path, value: &T) -> Result<(), MemoryError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let raw = serde_json::to_string_pretty(value)?;
    let tmp_path = temp_path_for(path);
    std::fs::write(&tmp_path, raw)?;
    std::fs::rename(&tmp_path, path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

fn temp_path_for(path: &Path) -> PathBuf {
    let unique = format!(
        ".{}.tmp-{}-{}",
        path.file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("state"),
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    path.with_file_name(unique)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_default_mempalace_store_from_config() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path();
        let mempal_dir = home.join(".mempalace").join("custom-palace");
        std::fs::create_dir_all(&mempal_dir).unwrap();
        std::fs::write(mempal_dir.join("chroma.sqlite3"), "").unwrap();
        std::fs::create_dir_all(home.join(".mempalace")).unwrap();
        std::fs::write(
            home.join(".mempalace").join("config.json"),
            serde_json::json!({
                "palace_path": mempal_dir.display().to_string()
            })
            .to_string(),
        )
        .unwrap();

        let original_home = std::env::var("HOME").ok();
        std::env::set_var("HOME", home);

        let detected = detect_mempalace_store().unwrap();
        assert_eq!(detected, mempal_dir);

        if let Some(value) = original_home {
            std::env::set_var("HOME", value);
        }
    }

    #[test]
    fn resolve_workspace_root_without_input_does_not_fallback_to_cwd() {
        let original = std::env::var("IRONMEM_WORKSPACE_ROOT").ok();
        std::env::remove_var("IRONMEM_WORKSPACE_ROOT");

        let resolved = resolve_workspace_root(None);

        if let Some(value) = original {
            std::env::set_var("IRONMEM_WORKSPACE_ROOT", value);
        }

        assert!(
            resolved.is_none(),
            "workspace auto-bootstrap should require an explicit workspace root"
        );
    }
}
