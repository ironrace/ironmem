use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier, Mutex};

use ironmem::bootstrap::ensure_bootstrapped;
use ironmem::config::{Config, EmbedMode, McpAccessMode};
use ironmem::mcp::app::App;
use sha2::{Digest, Sha256};

fn test_config(root: &Path) -> Config {
    Config {
        db_path: root.join("memory.sqlite3"),
        model_dir: root.join("noop-model"),
        model_dir_explicit: true,
        state_dir: root.join("hook_state"),
        mcp_access_mode: McpAccessMode::Trusted,
        embed_mode: EmbedMode::Noop,
    }
}

struct EnvGuard {
    key: &'static str,
    original: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
        let original = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, original }
    }

    fn remove(key: &'static str) -> Self {
        let original = std::env::var(key).ok();
        std::env::remove_var(key);
        Self { key, original }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        if let Some(value) = &self.original {
            std::env::set_var(self.key, value);
        } else {
            std::env::remove_var(self.key);
        }
    }
}

/// Tests that manipulate process-global env vars (HOME, IRONMEM_*) must not run
/// concurrently — concurrent drops of EnvGuard can unset IRONMEM_DISABLE_MIGRATION
/// while the other test's threads are inside ensure_bootstrapped, causing
/// detect_mempalace_store to find the real ~/.mempalace store and migrate it.
static ENV_MUTEX: Mutex<()> = Mutex::new(());

fn workspace_state_path(state_dir: &Path, workspace_root: &Path) -> PathBuf {
    let mut hasher = Sha256::new();
    hasher.update(workspace_root.to_string_lossy().as_bytes());
    let key = format!("{:x}", hasher.finalize());
    state_dir
        .join("workspaces")
        .join(format!("{}.json", &key[..16]))
}

#[test]
fn concurrent_bootstrap_on_same_workspace_completes_without_duplication() {
    let _env = ENV_MUTEX.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let _home = EnvGuard::set("HOME", &home);
    let _disable = EnvGuard::set("IRONMEM_DISABLE_MIGRATION", "1");
    let _migrate_from = EnvGuard::remove("IRONMEM_MIGRATE_FROM");
    let _mempalace = EnvGuard::remove("MEMPALACE_PALACE_PATH");
    let _mempal = EnvGuard::remove("MEMPAL_PALACE_PATH");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("README.md"), "# Workspace\n\nBootstrap me.").unwrap();

    let cfg = test_config(root);
    let barrier = Arc::new(Barrier::new(2));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let cfg = cfg.clone();
            let workspace = workspace.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let app = App::new(cfg).unwrap();
                barrier.wait();
                ensure_bootstrapped(&app, Some(workspace.as_path()))
            })
        })
        .collect();

    for handle in handles {
        handle.join().unwrap().unwrap();
    }

    let verifier = App::new(cfg).unwrap();
    assert_eq!(
        verifier.db.count_drawers(None).unwrap(),
        1,
        "bootstrap race should not duplicate indexed drawers"
    );
}

#[test]
fn malformed_workspace_state_is_ignored_and_recovery_remains_idempotent() {
    let _env = ENV_MUTEX.lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let _home = EnvGuard::set("HOME", &home);
    let _disable = EnvGuard::set("IRONMEM_DISABLE_MIGRATION", "1");
    let _migrate_from = EnvGuard::remove("IRONMEM_MIGRATE_FROM");
    let _mempalace = EnvGuard::remove("MEMPALACE_PALACE_PATH");
    let _mempal = EnvGuard::remove("MEMPAL_PALACE_PATH");
    let workspace = root.join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join("notes.md"),
        "Recovered after malformed state",
    )
    .unwrap();

    let cfg = test_config(root);
    let app = App::new(cfg.clone()).unwrap();
    ensure_bootstrapped(&app, Some(workspace.as_path())).unwrap();
    let count_before = app.db.count_drawers(None).unwrap();

    let state_path = workspace_state_path(&cfg.state_dir, &workspace);
    std::fs::create_dir_all(state_path.parent().unwrap()).unwrap();
    std::fs::write(&state_path, "{not-json").unwrap();

    let recovered = App::new(cfg).unwrap();
    ensure_bootstrapped(&recovered, Some(workspace.as_path())).unwrap();
    let count_after = recovered.db.count_drawers(None).unwrap();

    assert_eq!(
        count_after, count_before,
        "recovering from malformed state must not duplicate indexed data"
    );
}
