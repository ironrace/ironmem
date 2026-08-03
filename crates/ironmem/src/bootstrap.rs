//! Background bootstrap, workspace initialization, and stale-lock recovery.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::config::Config;
use crate::error::MemoryError;
use crate::mcp::app::App;
use crate::mcp::readiness::ReadinessGate;

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

pub const MEMORY_PROTOCOL: &str = "Before answering questions about prior work, decisions, project history, or people, check search or KG tools first. Write important durable decisions back to memory. For mutable current task/project context, use add_drawer with logical_key so the latest state overwrites stale copies instead of accumulating forever. Treat collab-plans, collab-task-lists, and collab-checkpoints as operational artifacts; prefer compact durable summaries for long-term recall and prune stale operational drawers with ironmem memory gc --dry-run before --apply.";

/// Write the current binary version to `state_dir/server.version`.
/// If the version changed since last run, log an upgrade notice to stderr.
/// Non-fatal: errors are silently ignored.
pub fn check_and_record_version(state_dir: &Path) {
    let version_file = state_dir.join("server.version");
    let current = env!("CARGO_PKG_VERSION");
    if let Ok(prev) = std::fs::read_to_string(&version_file) {
        let prev = prev.trim();
        if prev != current {
            tracing::info!("ironmem upgraded {prev} → {current}");
        }
    }
    let _ = std::fs::write(&version_file, current);
}

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

/// How long to wait for a contended lock before giving up.
const LOCK_TIMEOUT: Duration = Duration::from_secs(10);

/// How old a lock whose contents are not a PID must be before it is reclaimed.
/// Kept well under `LOCK_TIMEOUT` so recovery actually happens inside one acquire;
/// a grace longer than the timeout would deadlock exactly like the bug it fixes.
const UNREADABLE_LOCK_GRACE: Duration = Duration::from_secs(2);

/// Distinguishes the scratch files of concurrent acquirers inside one process.
static SCRATCH_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl BootstrapLock {
    fn acquire(state_dir: &Path) -> Result<Self, MemoryError> {
        std::fs::create_dir_all(state_dir)?;
        let path = state_dir.join("bootstrap.lock");
        let start = Instant::now();
        loop {
            match create_lock_file(&path) {
                Ok(()) => return Ok(Self { path }),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if lock_is_stale(&path) {
                        // Owner crashed, or the lock never got a PID written into it.
                        // Remove and retry immediately.
                        tracing::warn!("Removing stale bootstrap lock at {}", path.display());
                        let _ = std::fs::remove_file(&path);
                        continue;
                    }
                    if start.elapsed() > LOCK_TIMEOUT {
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

/// Create the lock file with our PID already in it, atomically.
///
/// The PID is written to a private scratch file first and only then linked into
/// place, so the lock is never observable in a contentless state. Creating the
/// lock by `create_new` and writing the PID afterwards leaves a window where a
/// kill (hook timeout, Ctrl-C) strands a 0-byte lock that no owner check can
/// attribute — which is unrecoverable without the staleness grace below.
///
/// Returns `ErrorKind::AlreadyExists` when another holder owns the lock.
fn create_lock_file(path: &Path) -> std::io::Result<()> {
    use std::io::Write;

    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    // Unique per acquirer, not merely per process: two threads bootstrapping
    // concurrently share a PID, and a PID-only name lets one thread truncate or
    // unlink the scratch file the other is mid-way through linking.
    let scratch = dir.join(format!(
        "bootstrap.lock.{}.{}.tmp",
        std::process::id(),
        SCRATCH_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));

    let write_pid = || -> std::io::Result<()> {
        let mut file = std::fs::File::create(&scratch)?;
        file.write_all(std::process::id().to_string().as_bytes())?;
        file.sync_all()
    };
    if let Err(error) = write_pid() {
        let _ = std::fs::remove_file(&scratch);
        return Err(error);
    }

    // `hard_link` fails with AlreadyExists if the lock is held — this is the
    // atomic create-if-absent step, and it publishes a fully-written file.
    let result = std::fs::hard_link(&scratch, path);
    let _ = std::fs::remove_file(&scratch);
    result
}

/// Whether an existing lock file can be reclaimed.
///
/// A lock naming a live process is never stale. A lock naming a dead process is.
/// A lock we cannot attribute at all — empty or garbage — is stale once it has
/// aged past [`UNREADABLE_LOCK_GRACE`]; without this, unparseable contents skip
/// every liveness check and the lock is held forever.
fn lock_is_stale(path: &Path) -> bool {
    if let Ok(raw) = std::fs::read_to_string(path) {
        if let Ok(owner_pid) = raw.trim().parse::<u32>() {
            return !process_is_alive(owner_pid);
        }
    }

    // Unattributable. Age it, and treat an unusable timestamp (missing metadata,
    // mtime in the future from clock skew) as stale too: a lock that can be
    // neither attributed nor aged is not worth deadlocking on. Mutual exclusion
    // still holds — the `hard_link` below decides a single winner among racers.
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .map_or(true, |modified| {
            modified
                .elapsed()
                .map_or(true, |age| age >= UNREADABLE_LOCK_GRACE)
        })
}

/// Returns true if the given PID has a live process on this system.
fn process_is_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        // SAFETY: kill(pid, 0) probes process existence without sending a signal.
        // Returns 0 if alive, ESRCH if the process does not exist.
        // EPERM means the process exists but we lack permission to signal it —
        // treat as alive (conservative; the lock is not stale).
        // `pid` is cast from u32 to pid_t (i32); values ≤ i32::MAX are safe.
        let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
        if result == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        true // Conservative: assume alive on non-Unix
    }
}

impl Drop for BootstrapLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Spawn a background thread that runs the full memory init (model load + bootstrap).
/// Always resolves `memory_ready` to a terminal state when done, so the serve loop
/// is never left waiting forever: `App::new` failing resolves the gate `Failed`
/// (write-shaped tools stay blocked/rejected — see `ReadinessGate` — until the process
/// restarts, since the embedder/index never came up); otherwise it resolves `Ready`,
/// regardless of whether the best-effort `ensure_bootstrapped` step below succeeded.
///
/// The thread opens its own `App` (its own DB connection). SQLite WAL handles
/// concurrent access from the serve loop's connection and this background connection.
/// Resolves the readiness gate `Failed` if the init thread exits without
/// having resolved it — including by PANIC.
///
/// Without this, a panic anywhere in init (a corrupt `model.onnx` making the
/// ONNX runtime panic rather than return `Err`, say) leaves the gate `Pending`
/// forever: the `JoinHandle` is dropped so nothing observes the unwind, `status`
/// reports `warming_up` for the life of the process, and `search` keeps
/// answering "available shortly" — exactly the dead-server-looks-slow lie the
/// terminal `Failed` state was introduced to prevent.
///
/// Resolution is first-wins, so on both normal paths (which resolve explicitly)
/// this drop is a no-op.
struct ResolveOnExit(Arc<ReadinessGate>);

impl Drop for ResolveOnExit {
    fn drop(&mut self) {
        self.0
            .resolve_failed(crate::mcp::readiness::STARTUP_FAILURE_CLIENT_REASON.to_string());
    }
}

pub fn run_background_memory_init(config: Config, memory_ready: Arc<ReadinessGate>) {
    std::thread::spawn(move || init_thread_body(config, memory_ready));
}

/// The init thread's body, factored out so tests can drive the REAL exit paths
/// rather than a reconstruction of them.
fn init_thread_body(config: Config, memory_ready: Arc<ReadinessGate>) {
    {
        // Armed before any fallible work, so no exit path can skip it.
        let _resolve_on_exit = ResolveOnExit(Arc::clone(&memory_ready));
        // Capture write permission before config is moved into App::new.
        let writes_allowed = config.mcp_access_mode.allows_writes();
        let workspace = resolve_workspace_root(None);
        let app = match App::new(config) {
            Ok(a) => a,
            Err(e) => {
                // Fail-closed: the embedder/index never came up, so write-shaped
                // tools must keep blocking/rejecting (via `wait_for_write`) rather
                // than silently unblocking against a half-initialized `App`.
                tracing::error!(
                    "Background memory init failed (App::new): {e}; write-shaped tools will \
                     stay blocked until the process restarts"
                );
                // No explicit resolve here: `ResolveOnExit` resolves `Failed`
                // on the way out. Deliberately the SOLE mechanism, so the guard
                // is load-bearing on a path a test can actually drive — a
                // second, redundant resolve would let the guard rot unnoticed.
                //
                // The gate's reason crosses the MCP client boundary verbatim
                // (see `STARTUP_FAILURE_CLIENT_REASON`), so `e` — which can
                // carry database paths and OS error text — stays in the log
                // line above and never in the reason.
                return;
            }
        };
        // Deliberately asymmetric with the `App::new` failure above: `app` itself came
        // up fine here, so the embedder/index are usable even if this best-effort
        // migration/mining step fails — resolve `Ready` either way rather than
        // permanently blocking writes over a non-fatal bootstrap error.
        if writes_allowed {
            // `catch_unwind` so a PANIC here is treated exactly like the `Err`
            // arm below. Without it the `ResolveOnExit` guard would fire and
            // resolve `Failed`, permanently disabling writes and making
            // `search` report a terminal failure — over a best-effort mining
            // step, on a server whose embedder and index are fully working.
            // That would silently invert the asymmetry documented above.
            let bootstrapped = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                ensure_bootstrapped(&app, workspace.as_deref())
            }));
            match bootstrapped {
                Ok(Ok(r)) => tracing::info!(
                    "Bootstrap complete (initialized={}, mine_ran={})",
                    r.initialized_store,
                    r.initial_mine_ran
                ),
                Ok(Err(e)) => tracing::error!("Bootstrap failed: {e}"),
                Err(_) => tracing::error!(
                    "Bootstrap panicked; continuing with a usable embedder and index"
                ),
            }
        } else {
            tracing::debug!("Skipping auto-bootstrap: MCP access mode does not allow writes");
        }
        memory_ready.resolve_ready();
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

    /// A panic in the init thread must still leave the gate TERMINAL.
    ///
    /// `run_background_memory_init` drops its `JoinHandle`, so an unwind is
    /// observed by nothing. If the gate stayed `Pending`, `status` would report
    /// `warming_up` forever and `search` would keep promising results
    /// "shortly" from a server that is never coming up — the precise pathology
    /// the terminal `Failed` state exists to prevent. Every write would also
    /// burn the full readiness timeout, once per call, forever.
    #[test]
    fn a_panicking_init_thread_still_resolves_the_gate() {
        let gate = Arc::new(ReadinessGate::new_pending());

        let thread_gate = Arc::clone(&gate);
        let handle = std::thread::spawn(move || {
            let _resolve_on_exit = ResolveOnExit(Arc::clone(&thread_gate));
            panic!("simulated ONNX session panic during model load");
        });
        assert!(handle.join().is_err(), "the thread must have panicked");

        assert!(
            !gate.is_ready(),
            "a panicked init must never report the server as ready"
        );
        assert!(
            matches!(
                gate.snapshot(),
                crate::mcp::readiness::ReadinessState::Failed(_)
            ),
            "a panicked init must leave the gate terminally Failed, not Pending — \
             got {:?}",
            gate.snapshot()
        );
    }

    /// Drives the REAL `init_thread_body` down its failure path, which is the
    /// only way to prove the guard is actually ARMED in production.
    ///
    /// `ResolveOnExit` is deliberately the sole resolver on that path, so
    /// deleting or sinking the arming line leaves the gate `Pending` and this
    /// test fails. A test that constructed the guard itself could not detect
    /// that — it would only re-test `Drop`.
    #[test]
    fn init_thread_body_resolves_the_gate_when_app_new_fails() {
        let dir = tempfile::tempdir().unwrap();
        // `db_path` points AT a directory, so `Database::open` cannot succeed.
        let config = Config {
            db_path: dir.path().to_path_buf(),
            model_dir: dir.path().join("models"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: crate::config::McpAccessMode::ReadOnly,
            embed_mode: crate::config::EmbedMode::Noop,
        };

        let gate = Arc::new(ReadinessGate::new_pending());
        init_thread_body(config, Arc::clone(&gate));

        assert!(
            !gate.is_ready(),
            "a failed init must never report the server as ready"
        );
        assert!(
            matches!(
                gate.snapshot(),
                crate::mcp::readiness::ReadinessState::Failed(_)
            ),
            "init must leave the gate terminally Failed, not Pending — got {:?}",
            gate.snapshot()
        );
    }

    /// The guard must not override a real resolution: first-wins means the
    /// normal success path stays `Ready` even though the guard runs on drop.
    #[test]
    fn resolve_on_exit_does_not_override_a_successful_resolution() {
        let gate = Arc::new(ReadinessGate::new_pending());
        {
            let _resolve_on_exit = ResolveOnExit(Arc::clone(&gate));
            gate.resolve_ready();
        }
        assert!(
            gate.is_ready(),
            "the drop guard must not downgrade an already-Ready gate"
        );
    }

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

    #[test]
    #[cfg(unix)]
    fn stale_bootstrap_lock_is_recovered_when_owner_pid_is_gone() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("bootstrap.lock");

        // Obtain a real dead PID: spawn a no-op child, wait for it to exit, then use its PID.
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let dead_pid = child.id();
        child.wait().unwrap();
        std::fs::write(&lock_path, dead_pid.to_string()).unwrap();

        // Acquiring the lock should succeed by removing the stale lock file.
        let lock = BootstrapLock::acquire(temp.path()).unwrap();
        drop(lock);

        assert!(!lock_path.exists(), "lock file should have been cleaned up");
    }

    /// Backdate a file's mtime so age-based staleness can be tested without sleeping.
    fn age_file(path: &Path, by: Duration) {
        let modified = std::time::SystemTime::now() - by;
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(modified)
            .unwrap();
    }

    #[test]
    fn aged_empty_bootstrap_lock_is_reclaimed() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("bootstrap.lock");

        // A kill between lock creation and the PID write strands a 0-byte lock.
        std::fs::write(&lock_path, b"").unwrap();
        age_file(&lock_path, UNREADABLE_LOCK_GRACE * 2);

        let lock = BootstrapLock::acquire(temp.path()).unwrap();
        drop(lock);

        assert!(!lock_path.exists(), "lock file should have been cleaned up");
    }

    #[test]
    fn aged_garbage_bootstrap_lock_is_reclaimed() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("bootstrap.lock");

        std::fs::write(&lock_path, b"not-a-pid").unwrap();
        age_file(&lock_path, UNREADABLE_LOCK_GRACE * 2);

        let lock = BootstrapLock::acquire(temp.path()).unwrap();
        drop(lock);

        assert!(!lock_path.exists(), "lock file should have been cleaned up");
    }

    #[test]
    fn freshly_created_empty_lock_is_not_reclaimed_within_the_grace() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("bootstrap.lock");

        std::fs::write(&lock_path, b"").unwrap();

        assert!(
            !lock_is_stale(&lock_path),
            "an unattributable lock must age past the grace before being reclaimed"
        );
    }

    #[test]
    fn lock_owned_by_a_live_process_is_not_stale() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("bootstrap.lock");

        std::fs::write(&lock_path, std::process::id().to_string()).unwrap();
        // Even aged well past the grace, a live owner keeps the lock.
        age_file(&lock_path, UNREADABLE_LOCK_GRACE * 10);

        assert!(
            !lock_is_stale(&lock_path),
            "live owner must retain the lock"
        );
    }

    #[test]
    fn lock_file_is_never_published_without_a_pid() {
        let temp = tempfile::tempdir().unwrap();
        let lock_path = temp.path().join("bootstrap.lock");

        let lock = BootstrapLock::acquire(temp.path()).unwrap();

        let contents = std::fs::read_to_string(&lock_path).unwrap();
        assert_eq!(
            contents.trim().parse::<u32>().ok(),
            Some(std::process::id()),
            "lock must carry our PID the moment it exists"
        );
        drop(lock);
    }

    #[test]
    fn acquire_leaves_no_scratch_file_behind() {
        let temp = tempfile::tempdir().unwrap();

        let lock = BootstrapLock::acquire(temp.path()).unwrap();
        drop(lock);

        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "scratch file should be cleaned up, found: {leftovers:?}"
        );
    }

    #[test]
    fn concurrent_acquirers_in_one_process_never_collide_on_scratch() {
        let temp = tempfile::tempdir().unwrap();
        let threads = 8;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(threads));

        // Threads share a PID, so a PID-only scratch name lets one thread unlink
        // the file another is linking — surfacing as NotFound, not AlreadyExists.
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                let dir = temp.path().to_path_buf();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    create_lock_file(&dir.join("bootstrap.lock"))
                })
            })
            .collect();

        let mut winners = 0;
        for handle in handles {
            match handle.join().unwrap() {
                Ok(()) => winners += 1,
                Err(error) => assert_eq!(
                    error.kind(),
                    std::io::ErrorKind::AlreadyExists,
                    "losers must lose by contention, not by clobbering each other"
                ),
            }
        }
        assert_eq!(winners, 1, "exactly one acquirer may take the lock");

        let leftovers: Vec<_> = std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "no scratch files should survive, found: {leftovers:?}"
        );
    }

    #[test]
    fn held_lock_blocks_a_second_acquire() {
        let temp = tempfile::tempdir().unwrap();

        let held = BootstrapLock::acquire(temp.path()).unwrap();

        // Our own PID is alive, so the contender must never reclaim it.
        assert!(
            create_lock_file(&temp.path().join("bootstrap.lock"))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::AlreadyExists),
            "a held lock must reject a second creation"
        );
        drop(held);
    }
}
