//! `ironmem doctor` — local setup diagnostics.
//!
//! Validates the pieces a fresh install needs (binary, database + schema, model
//! cache, MCP access mode, warmup readiness, configured harnesses) and reports
//! each as a [`Check`]. The command is diagnose-only: it never modifies user
//! config. Only a [`CheckStatus::Error`] is a *blocking* setup failure and
//! drives a non-zero exit; warnings and info lines are advisory.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Serialize;

use crate::config::{Config, EmbedMode, McpAccessMode};
use crate::db::schema::{Database, LATEST_SCHEMA_VERSION};

mod render;
pub use render::render_text;

/// Severity of a single diagnostic check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// Healthy.
    Ok,
    /// Informational — nothing wrong, nothing to fix.
    Info,
    /// Non-blocking problem the user should look at.
    Warn,
    /// Blocking setup failure — drives a non-zero exit code.
    Error,
}

impl CheckStatus {
    /// Only [`CheckStatus::Error`] is a blocking setup failure. Written as an
    /// exhaustive match (not `matches!`) so adding a future severity variant
    /// forces a deliberate blocking/non-blocking decision here at compile time.
    pub fn is_blocking(self) -> bool {
        match self {
            CheckStatus::Error => true,
            CheckStatus::Ok | CheckStatus::Info | CheckStatus::Warn => false,
        }
    }
}

/// One diagnostic line.
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    /// Stable machine-readable key (e.g. `"database"`).
    pub name: &'static str,
    /// Severity.
    pub status: CheckStatus,
    /// Human-readable one-line summary.
    pub summary: String,
    /// Optional actionable remediation hint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

impl Check {
    fn new(name: &'static str, status: CheckStatus, summary: impl Into<String>) -> Self {
        Self {
            name,
            status,
            summary: summary.into(),
            hint: None,
        }
    }

    fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }
}

/// Aggregate diagnostic result.
#[derive(Debug, Clone, Serialize)]
pub struct DoctorReport {
    /// Checks in display order.
    pub checks: Vec<Check>,
}

impl DoctorReport {
    /// True when any check is a blocking setup failure (drives exit code).
    pub fn has_blocking(&self) -> bool {
        self.checks.iter().any(|c| c.status.is_blocking())
    }
}

/// Run every diagnostic against the loaded configuration and the current
/// environment (home directory, `CODEX_HOME`). Never fails: every problem is
/// captured as a [`Check`] rather than returned as an error.
pub fn run_doctor(cfg: &Config) -> DoctorReport {
    let home = dirs::home_dir();

    let binary = check_binary(env!("IRONMEM_VERSION"));
    let database = check_database(&cfg.db_path);
    let model = check_model_cache(&cfg.model_dir, cfg.embed_mode);
    let mcp = check_mcp_mode(cfg.mcp_access_mode);
    let warmup = check_warmup(database.status, model.status);

    let mut checks = vec![binary, database, model, mcp, warmup];
    checks.extend(harness_checks(
        home.as_deref(),
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        crate::harness::REGISTRY,
    ));

    DoctorReport { checks }
}

/// Extend [`run_doctor`]'s report with the shared-daemon health probe and
/// auto-spawn configuration (#190 Task 14). Async because probing the daemon
/// needs a real Unix-socket connect + `initialize` round trip; every other
/// check in `run_doctor` is synchronous file/DB I/O and is unaffected — this
/// wrapper exists so `run_doctor` itself, and its many existing synchronous
/// callers/tests, stay untouched.
#[cfg(unix)]
pub async fn run_doctor_with_daemon(cfg: &Config) -> DoctorReport {
    let mut report = run_doctor(cfg);
    let socket_path = cfg.daemon_socket_path();
    let health =
        crate::mcp::daemon::probe_daemon_health(&socket_path, Duration::from_millis(500)).await;
    report.checks.push(daemon_check(&socket_path, health));
    report.checks.push(autospawn_check(cfg));
    report
}

/// Non-Unix fallback (#190 Task 10): no Unix-domain-socket transport exists on
/// this platform, so the daemon check reports that directly instead of
/// attempting a probe that could never succeed.
#[cfg(not(unix))]
pub async fn run_doctor_with_daemon(cfg: &Config) -> DoctorReport {
    let mut report = run_doctor(cfg);
    report.checks.push(Check::new(
        "daemon",
        CheckStatus::Info,
        "shared daemon not supported on this platform (Unix-domain sockets only)",
    ));
    report.checks.push(autospawn_check(cfg));
    report
}

#[cfg(unix)]
fn daemon_check(socket_path: &Path, health: crate::mcp::daemon::DaemonHealth) -> Check {
    match health {
        crate::mcp::daemon::DaemonHealth::Reachable => Check::new(
            "daemon",
            CheckStatus::Ok,
            format!("shared daemon reachable at {}", socket_path.display()),
        ),
        crate::mcp::daemon::DaemonHealth::RejectedHandshake => Check::new(
            "daemon",
            CheckStatus::Warn,
            format!(
                "shared daemon at {} is running but rejected the health-probe handshake",
                socket_path.display()
            ),
        )
        .with_hint("it may be stale; consider restarting it"),
        crate::mcp::daemon::DaemonHealth::Unreachable => Check::new(
            "daemon",
            CheckStatus::Info,
            format!("no shared daemon running at {}", socket_path.display()),
        )
        .with_hint("a `serve --connect` proxy will auto-spawn one on demand, unless disabled"),
    }
}

/// Report whether `serve --connect` auto-spawn is currently enabled
/// (`Config::daemon_autospawn_enabled`, `IRONMEM_NO_DAEMON`).
fn autospawn_check(cfg: &Config) -> Check {
    if cfg.daemon_autospawn_enabled() {
        Check::new(
            "daemon_autospawn",
            CheckStatus::Info,
            "daemon auto-spawn: enabled",
        )
    } else {
        Check::new(
            "daemon_autospawn",
            CheckStatus::Info,
            "daemon auto-spawn: disabled (IRONMEM_NO_DAEMON)",
        )
    }
}

/// Build the per-harness doctor checks by iterating the registry.
///
/// Detection strategy is dispatched per harness `id`:
/// - `"codex"` → line-match TOML via `codex_config_path`.
/// - `"claude"`, `"gemini"`, `"grok"`, `"muse"` → one shared JSON parse
///   ([`detect_json_mcpservers`]) of the file
///   [`crate::launcher::json_mcpservers_config_path_for`] resolves for the
///   id — the same table the launcher writes through — matching the
///   `mcpServers` OBJECT for an `"ironmem"` entry. Muse's object shape is
///   proven live on Muse Code 1.0.2 (`muse exec` completes `initialize` +
///   `tools/list` on it, while an array-shaped `mcpServers` is silently
///   ignored); Gemini's is documented; Grok's is best-effort (see the
///   resolver). ironmem reads and writes only the `mcpServers` key.
/// - Any other id → an `Info` check noting detection is not yet implemented.
///
/// `xdg_config_home` is the observed `XDG_CONFIG_HOME` (only Muse honours
/// it), passed in rather than read here so tests can inject it without
/// mutating the process-global environment under other threads.
///
/// Check keys are stable `"harness_<id>"` strings.  For the registry's own
/// ids the key is a string literal (`&'static str`).  For any additional id
/// the key is produced by `Box::leak` — acceptable because `ironmem doctor`
/// runs once per process and the leaked bytes are tiny and bounded by the
/// (small, static) registry size.
///
/// Takes an explicit registry slice so tests can inject a synthetic 3-entry
/// slice without mutating global state.
fn harness_checks(
    home: Option<&Path>,
    xdg_config_home: Option<&OsStr>,
    registry: &[crate::harness::HarnessSpec],
) -> Vec<Check> {
    registry
        .iter()
        .map(|spec| {
            // Stable check key: literal for the registry's ids; Box::leak for
            // any additional registry entry (see doc comment).
            let key: &'static str = match spec.id {
                "claude" => "harness_claude",
                "codex" => "harness_codex",
                "grok" => "harness_grok",
                "gemini" => "harness_gemini",
                "muse" => "harness_muse",
                other => Box::leak(format!("harness_{other}").into_boxed_str()),
            };
            let skipped = || {
                Check::new(
                    key,
                    CheckStatus::Info,
                    format!("{}: home directory unknown, skipped", spec.display_name),
                )
            };

            match spec.id {
                "codex" => match codex_config_path(home) {
                    Some(path) => {
                        let state = detect_codex(&path);
                        with_proxy_wiring_note(
                            harness_check(key, spec.display_name, &path, state.clone()),
                            &state,
                            spec.id,
                            || codex_proxy_wiring(&path),
                        )
                    }
                    None => skipped(),
                },
                "claude" | "gemini" | "grok" | "muse" => {
                    let path = crate::launcher::json_mcpservers_config_path_for(
                        spec.id,
                        home.map(Path::to_path_buf),
                        xdg_config_home.map(OsStr::to_os_string),
                    );
                    match path {
                        Some(path) => {
                            let (state, wired) = detect_json_mcpservers(&path);
                            let check = harness_check(key, spec.display_name, &path, state.clone());
                            // scripts/install-ironmem.sh wires only Claude and
                            // Codex, so every other JSON harness points at its
                            // launcher instead of the script.
                            let check = if state == HarnessState::NotRegistered && spec.id != "claude"
                            {
                                check.with_hint(format!(
                                    "run `ironmem {}` to register it (scripts/install-ironmem.sh covers only Claude and Codex)",
                                    spec.id
                                ))
                            } else {
                                check
                            };
                            with_proxy_wiring_note(check, &state, spec.id, || wired)
                        }
                        None => skipped(),
                    }
                }
                _ => Check::new(
                    key,
                    CheckStatus::Info,
                    format!(
                        "{}: registration detection not yet implemented",
                        spec.display_name
                    ),
                ),
            }
        })
        .collect()
}

/// Append a "wired with the shared-daemon proxy command?" note (#190 Task 14)
/// to `check`'s summary, but ONLY when `state` is [`HarnessState::Registered`]
/// — an absent/unreadable/malformed config has nothing meaningful to say
/// about wiring. `detect_wiring` is called lazily (only when registered) and
/// may return `None` if the config couldn't be re-read/parsed for this
/// narrower question, in which case the summary is left as-is.
fn with_proxy_wiring_note(
    mut check: Check,
    state: &HarnessState,
    launcher: &str,
    detect_wiring: impl FnOnce() -> Option<bool>,
) -> Check {
    if *state == HarnessState::Registered {
        if let Some(is_wired) = detect_wiring() {
            check.summary = format!(
                "{}; {}",
                check.summary,
                proxy_wiring_note(is_wired, launcher)
            );
        }
    }
    check
}

fn proxy_wiring_note(is_wired: bool, launcher: &str) -> String {
    if is_wired {
        "wired with the shared-daemon proxy command".to_string()
    } else {
        format!("wired with the legacy bare `serve` command (upgrade available via `ironmem {launcher}`)")
    }
}

/// Whether Codex's registered `ironmem` entry uses the shared-daemon proxy
/// command. Scoped to the `[mcp_servers.ironmem]` section specifically (not a
/// whole-file substring search) so an unrelated section mentioning
/// `--connect` can never produce a false positive.
fn codex_proxy_wiring(path: &Path) -> Option<bool> {
    let raw = std::fs::read_to_string(path).ok()?;
    let start = raw.find("[mcp_servers.ironmem]")?;
    let section = &raw[start..];
    let end = section[1..]
        .find("\n[")
        .map(|i| i + 1)
        .unwrap_or(section.len());
    Some(section[..end].contains("--connect"))
}

fn check_binary(version: &str) -> Check {
    Check::new("binary", CheckStatus::Ok, format!("ironmem {version}"))
}

/// Inspect the database file and its applied schema version.
fn check_database(db_path: &Path) -> Check {
    let shown = db_path.display();
    if !db_path.exists() {
        return Check::new(
            "database",
            CheckStatus::Warn,
            format!("database not found at {shown}"),
        )
        .with_hint("run `ironmem init` (or start the MCP server) to create it");
    }

    // Open the existing file without migrating or creating it: a diagnostic
    // must not mutate the store (no WAL switch, no chmod, no schema changes).
    let db = match Database::open_with_busy_timeout(db_path, Duration::from_millis(500)) {
        Ok(db) => db,
        Err(e) if is_db_busy(&e) => return database_busy_check(&shown),
        Err(e) => {
            return Check::new(
                "database",
                CheckStatus::Error,
                format!("cannot open database at {shown}: {e}"),
            )
            .with_hint("the file may be corrupt or locked by another process");
        }
    };

    match db.schema_version() {
        Ok(v) if v == LATEST_SCHEMA_VERSION => Check::new(
            "database",
            CheckStatus::Ok,
            format!("schema v{v} (current) at {shown}"),
        ),
        Ok(v) if v < LATEST_SCHEMA_VERSION => Check::new(
            "database",
            CheckStatus::Warn,
            format!("schema v{v} is behind current v{LATEST_SCHEMA_VERSION} at {shown}"),
        )
        .with_hint("migrations apply automatically next time the server starts"),
        Ok(v) => Check::new(
            "database",
            CheckStatus::Warn,
            format!("schema v{v} is newer than this binary's v{LATEST_SCHEMA_VERSION} at {shown}"),
        )
        .with_hint("upgrade ironmem to match the database"),
        // A transient busy/locked condition means the store is healthy but
        // in use — report it as retryable, not as a blocking corruption.
        Err(e) if is_db_busy(&e) => database_busy_check(&shown),
        Err(e) => Check::new(
            "database",
            CheckStatus::Error,
            format!("cannot read schema version at {shown}: {e}"),
        )
        .with_hint("the database appears initialized but unreadable; it may be corrupt"),
    }
}

/// The non-blocking `[WARN]` for a database that is healthy but momentarily
/// locked by another process (e.g. the MCP server mid-write).
fn database_busy_check(shown: &std::path::Display) -> Check {
    Check::new(
        "database",
        CheckStatus::Warn,
        format!("database is in use by another process at {shown}"),
    )
    .with_hint("a running ironmem server holds the lock; re-run when it is idle")
}

/// True when a [`MemoryError`] is a transient SQLite busy/locked condition
/// rather than a structural problem — so an in-use database is reported as a
/// retryable warning instead of a blocking corruption error.
fn is_db_busy(err: &crate::MemoryError) -> bool {
    matches!(
        err,
        crate::MemoryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked,
                ..
            },
            _,
        ))
    )
}

/// Inspect the embedding-model cache.
fn check_model_cache(model_dir: &Path, embed_mode: EmbedMode) -> Check {
    if matches!(embed_mode, EmbedMode::Noop) {
        return Check::new(
            "model",
            CheckStatus::Info,
            "noop embed mode — embedding model not required",
        );
    }

    let shown = model_dir.display();
    use ironrace_embed::embedder::ModelStatus;
    match ironrace_embed::embedder::model_status(model_dir) {
        ModelStatus::Ready => Check::new(
            "model",
            CheckStatus::Ok,
            format!("embedding model present and verified at {shown}"),
        ),
        ModelStatus::Missing => Check::new(
            "model",
            CheckStatus::Error,
            format!("embedding model not found at {shown}"),
        )
        .with_hint("run `ironmem setup` to download it"),
        ModelStatus::Corrupt => Check::new(
            "model",
            CheckStatus::Error,
            format!("embedding model failed checksum verification at {shown}"),
        )
        .with_hint("delete the directory and re-run `ironmem setup`"),
        ModelStatus::Unreadable(e) => Check::new(
            "model",
            CheckStatus::Error,
            format!("embedding model present but unreadable at {shown}: {e}"),
        )
        .with_hint("check file permissions on the model directory"),
    }
}

fn check_mcp_mode(mode: McpAccessMode) -> Check {
    let (label, writes) = match mode {
        McpAccessMode::Trusted => ("trusted", "writes allowed"),
        McpAccessMode::ReadOnly => ("read-only", "writes blocked"),
        McpAccessMode::Restricted => ("restricted", "writes blocked, sensitive content redacted"),
    };
    Check::new(
        "mcp_access",
        CheckStatus::Info,
        format!("MCP access mode: {label} ({writes})"),
    )
}

/// Derived readiness: the server can warm up and serve once the database is
/// reachable and the embedding model is usable (or not required).
fn check_warmup(database: CheckStatus, model: CheckStatus) -> Check {
    let db_ok = !database.is_blocking();
    let model_ok = !model.is_blocking();
    if db_ok && model_ok {
        Check::new(
            "warmup",
            CheckStatus::Ok,
            "ready to serve — database and model are usable",
        )
    } else {
        Check::new(
            "warmup",
            CheckStatus::Warn,
            "not ready to serve — resolve the blocking checks above",
        )
    }
}

/// Resolution of a harness config file's registration state. Distinguishing
/// "absent" from "present but unreadable/malformed" is deliberate: collapsing
/// them would report a config we simply could not inspect as "no config".
#[derive(Debug, Clone, PartialEq, Eq)]
enum HarnessState {
    /// The config file does not exist.
    Absent,
    /// The config exists and registers the `ironmem` MCP server.
    Registered,
    /// The config exists but does not register `ironmem`.
    NotRegistered,
    /// The config exists but could not be read (permissions, I/O, non-UTF-8).
    Unreadable(String),
    /// The config exists and was read but could not be parsed.
    Malformed(String),
}

/// Read a config file, mapping a not-found error to [`HarnessState::Absent`] and
/// any other read error to [`HarnessState::Unreadable`]. Returns the file
/// contents on success.
fn read_harness_config(path: &Path) -> Result<String, HarnessState> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(raw),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(HarnessState::Absent),
        Err(e) => Err(HarnessState::Unreadable(e.to_string())),
    }
}

/// Resolve registration in a Claude-shaped `mcpServers` JSON config
/// (`~/.claude.json`, Gemini/Grok `settings.json`, Muse `settings.json`)
/// with ONE read + parse, returning the state and — when `Registered` —
/// whether the `ironmem` entry is wired with the shared-daemon proxy command
/// (`args[1] == "--connect"`; `None` when `args` is absent or not an array).
///
/// Shape verdicts mirror the writer (`mcp_setup::ensure_json_mcpservers_registered`),
/// which refuses the same inputs: a non-object root, a present-but-non-object
/// `mcpServers`, or a non-object `mcpServers.ironmem` entry all report
/// `Malformed` rather than `NotRegistered`/`Registered`, so a schema drift is
/// visible instead of looking like "not installed" or "all good". Only the
/// `mcpServers` key is consulted — Muse also accepts an `mcp_servers` alias,
/// but ironmem neither reads nor writes it, so doctor and the launcher agree
/// about one file. Wiring is scoped to the `ironmem` entry so a sibling
/// entry mentioning `--connect` can never produce a false positive.
fn detect_json_mcpservers(path: &Path) -> (HarnessState, Option<bool>) {
    let raw = match read_harness_config(path) {
        Ok(raw) => raw,
        Err(state) => return (state, None),
    };
    let v: serde_json::Value = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => return (HarnessState::Malformed(e.to_string()), None),
    };
    let Some(root) = v.as_object() else {
        return (
            HarnessState::Malformed("root is not a JSON object".into()),
            None,
        );
    };
    let Some(servers) = root.get("mcpServers") else {
        return (HarnessState::NotRegistered, None);
    };
    let Some(servers) = servers.as_object() else {
        return (
            HarnessState::Malformed("mcpServers is not an object".into()),
            None,
        );
    };
    let Some(entry) = servers.get("ironmem") else {
        return (HarnessState::NotRegistered, None);
    };
    if !entry.is_object() {
        return (
            HarnessState::Malformed("mcpServers.ironmem is not an object".into()),
            None,
        );
    }
    let wired = entry
        .get("args")
        .and_then(|a| a.as_array())
        .map(|args| args.get(1).and_then(|a| a.as_str()) == Some("--connect"));
    (HarnessState::Registered, wired)
}

/// Resolve Codex registration from `config.toml` via line-based detection of the
/// `[mcp_servers.ironmem]` section header (matching the install script rather
/// than a full TOML parse, so there is no "malformed" outcome).
fn detect_codex(path: &Path) -> HarnessState {
    let raw = match read_harness_config(path) {
        Ok(raw) => raw,
        Err(state) => return state,
    };
    if raw
        .lines()
        .any(|line| line.trim() == "[mcp_servers.ironmem]")
    {
        HarnessState::Registered
    } else {
        HarnessState::NotRegistered
    }
}

/// Build the harness [`Check`] from a resolved [`HarnessState`]. `label` is the
/// harness display name (e.g. `"Claude Code"`); `config` is the path inspected.
fn harness_check(name: &'static str, label: &str, config: &Path, state: HarnessState) -> Check {
    let shown = config.display();
    match state {
        HarnessState::Absent => Check::new(
            name,
            CheckStatus::Info,
            format!("{label}: no config at {shown}"),
        ),
        HarnessState::Registered => Check::new(
            name,
            CheckStatus::Ok,
            format!("{label}: ironmem MCP server registered in {shown}"),
        ),
        HarnessState::NotRegistered => Check::new(
            name,
            CheckStatus::Warn,
            format!("{label}: config present but ironmem MCP server not registered"),
        )
        .with_hint("run scripts/install-ironmem.sh to register it"),
        HarnessState::Unreadable(e) => Check::new(
            name,
            CheckStatus::Warn,
            format!("{label}: config present at {shown} but unreadable: {e}"),
        )
        .with_hint("check file permissions and encoding"),
        HarnessState::Malformed(e) => Check::new(
            name,
            CheckStatus::Warn,
            format!("{label}: config at {shown} is malformed: {e}"),
        )
        .with_hint("fix the config file by hand, then re-check"),
    }
}

/// Resolve the Codex config path from `$CODEX_HOME` (default `~/.codex`),
/// ignoring an empty `CODEX_HOME`. `None` when no home is determinable.
fn codex_config_path(home: Option<&Path>) -> Option<PathBuf> {
    let codex_home = match std::env::var_os("CODEX_HOME") {
        Some(v) if !v.is_empty() => PathBuf::from(v),
        _ => home?.join(".codex"),
    };
    Some(codex_home.join("config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_status_is_blocking_others_are_not() {
        assert!(CheckStatus::Error.is_blocking());
        assert!(!CheckStatus::Warn.is_blocking());
        assert!(!CheckStatus::Info.is_blocking());
        assert!(!CheckStatus::Ok.is_blocking());
    }

    #[test]
    fn report_has_blocking_only_with_an_error_check() {
        let ok = DoctorReport {
            checks: vec![Check::new("a", CheckStatus::Warn, "w")],
        };
        assert!(!ok.has_blocking());
        let bad = DoctorReport {
            checks: vec![Check::new("a", CheckStatus::Error, "e")],
        };
        assert!(bad.has_blocking());
    }

    #[test]
    fn binary_check_reports_version_and_is_ok() {
        let c = check_binary("1.2.3");
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(c.summary.contains("1.2.3"));
    }

    #[test]
    fn database_missing_file_is_warn_not_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = check_database(&dir.path().join("nope.sqlite3"));
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.hint.is_some());
    }

    #[test]
    fn database_migrated_is_ok_with_current_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        Database::open(&path).unwrap().migrate().unwrap();
        let c = check_database(&path);
        assert_eq!(c.status, CheckStatus::Ok);
        assert!(c.summary.contains(&format!("v{LATEST_SCHEMA_VERSION}")));
    }

    #[test]
    fn database_unreadable_schema_is_blocking_error() {
        // A file that exists but is not a valid schema'd DB: create the file
        // empty so it opens but has no schema_version table.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty.sqlite3");
        std::fs::write(&path, b"").unwrap();
        let c = check_database(&path);
        assert_eq!(c.status, CheckStatus::Error);
    }

    #[test]
    fn database_behind_current_schema_is_warn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        Database::open(&path).unwrap().migrate().unwrap();
        // Force the recorded version below the binary's latest.
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute("DELETE FROM schema_version", []).unwrap();
        conn.execute("INSERT INTO schema_version (version) VALUES (7)", [])
            .unwrap();
        drop(conn);
        let c = check_database(&path);
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.summary.contains("behind"));
    }

    #[test]
    fn database_ahead_of_binary_schema_is_warn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.sqlite3");
        Database::open(&path).unwrap().migrate().unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute(
            "INSERT INTO schema_version (version) VALUES (?1)",
            [LATEST_SCHEMA_VERSION + 1],
        )
        .unwrap();
        drop(conn);
        let c = check_database(&path);
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.summary.contains("newer"));
    }

    #[test]
    fn is_db_busy_classifies_only_busy_and_locked() {
        let busy = crate::MemoryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(5), // SQLITE_BUSY
            None,
        ));
        let locked = crate::MemoryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(6), // SQLITE_LOCKED
            None,
        ));
        let corrupt = crate::MemoryError::Db(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(11), // SQLITE_CORRUPT
            None,
        ));
        assert!(is_db_busy(&busy));
        assert!(is_db_busy(&locked));
        assert!(!is_db_busy(&corrupt));
        assert!(!is_db_busy(&crate::MemoryError::Validation("x".into())));
    }

    #[test]
    fn database_busy_is_warn_not_blocking() {
        let path = std::path::Path::new("/tmp/in-use.sqlite3");
        let c = database_busy_check(&path.display());
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(!c.status.is_blocking());
        assert!(c.summary.contains("in use"));
    }

    #[test]
    fn model_check_skipped_in_noop_mode() {
        let dir = tempfile::tempdir().unwrap();
        let c = check_model_cache(dir.path(), EmbedMode::Noop);
        assert_eq!(c.status, CheckStatus::Info);
    }

    #[test]
    fn model_missing_in_real_mode_is_blocking_error() {
        let dir = tempfile::tempdir().unwrap();
        let c = check_model_cache(&dir.path().join("models"), EmbedMode::Real);
        assert_eq!(c.status, CheckStatus::Error);
        assert!(c.hint.unwrap().contains("setup"));
    }

    #[test]
    fn model_corrupt_in_real_mode_is_blocking_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("model.onnx"), b"bad").unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"bad").unwrap();
        let c = check_model_cache(dir.path(), EmbedMode::Real);
        assert_eq!(c.status, CheckStatus::Error);
        assert!(c.summary.contains("checksum"));
        assert!(c.hint.unwrap().contains("delete"));
    }

    #[test]
    fn model_unreadable_in_real_mode_is_blocking_error_with_permissions_hint() {
        // model.onnx present (so not Missing) but a directory → read fails →
        // Unreadable, which must not be reported as Corrupt/"delete it".
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("model.onnx")).unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), b"present").unwrap();
        let c = check_model_cache(dir.path(), EmbedMode::Real);
        assert_eq!(c.status, CheckStatus::Error);
        assert!(c.summary.contains("unreadable"));
        assert!(c.hint.unwrap().contains("permissions"));
    }

    #[test]
    fn mcp_mode_reports_each_variant_as_info() {
        for mode in [
            McpAccessMode::Trusted,
            McpAccessMode::ReadOnly,
            McpAccessMode::Restricted,
        ] {
            let c = check_mcp_mode(mode);
            assert_eq!(c.status, CheckStatus::Info);
        }
        assert!(check_mcp_mode(McpAccessMode::ReadOnly)
            .summary
            .contains("read-only"));
        assert!(check_mcp_mode(McpAccessMode::Trusted)
            .summary
            .contains("trusted"));
    }

    #[test]
    fn warmup_ok_only_when_neither_blocks() {
        assert_eq!(
            check_warmup(CheckStatus::Ok, CheckStatus::Info).status,
            CheckStatus::Ok
        );
        assert_eq!(
            check_warmup(CheckStatus::Error, CheckStatus::Ok).status,
            CheckStatus::Warn
        );
        assert_eq!(
            check_warmup(CheckStatus::Ok, CheckStatus::Error).status,
            CheckStatus::Warn
        );
    }

    #[test]
    fn detect_json_mcpservers_classifies_every_state() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        assert_eq!(detect_json_mcpservers(&path), (HarnessState::Absent, None));
        std::fs::write(
            &path,
            r#"{"mcpServers":{"ironmem":{"command":"ironmem","args":["serve","--connect","/tmp/d.sock"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_json_mcpservers(&path),
            (HarnessState::Registered, Some(true))
        );
        std::fs::write(
            &path,
            r#"{"mcpServers":{"ironmem":{"command":"ironmem","args":["serve"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_json_mcpservers(&path),
            (HarnessState::Registered, Some(false))
        );
        // Registered but no args at all: registered, wiring unknown.
        std::fs::write(&path, r#"{"mcpServers":{"ironmem":{"command":"ironmem"}}}"#).unwrap();
        assert_eq!(
            detect_json_mcpservers(&path),
            (HarnessState::Registered, None)
        );
        std::fs::write(&path, r#"{"mcpServers":{}}"#).unwrap();
        assert_eq!(
            detect_json_mcpservers(&path),
            (HarnessState::NotRegistered, None)
        );
        // Missing mcpServers key is "not registered", not malformed.
        std::fs::write(&path, r#"{"theme":"dark"}"#).unwrap();
        assert_eq!(
            detect_json_mcpservers(&path),
            (HarnessState::NotRegistered, None)
        );
        // Shapes the writer refuses are Malformed, never a silent verdict:
        // the plugin-manifest ARRAY shape (which Muse ignores in settings),
        // a scalar, a non-object ironmem entry, and a non-object root.
        for (raw, needle) in [
            (
                r#"{"mcpServers":[{"id":"ironmem"}]}"#,
                "mcpServers is not an object",
            ),
            (r#"{"mcpServers":5}"#, "mcpServers is not an object"),
            (
                r#"{"mcpServers":{"ironmem":"serve"}}"#,
                "mcpServers.ironmem is not an object",
            ),
            (
                r#"{"mcpServers":{"ironmem":null}}"#,
                "mcpServers.ironmem is not an object",
            ),
            ("[]", "root is not a JSON object"),
        ] {
            std::fs::write(&path, raw).unwrap();
            match detect_json_mcpservers(&path) {
                (HarnessState::Malformed(msg), None) => {
                    assert!(msg.contains(needle), "{raw}: got {msg}")
                }
                other => panic!("{raw}: expected Malformed, got {other:?}"),
            }
        }
        // Malformed JSON must be distinguished from "not registered".
        std::fs::write(&path, "{ not valid json").unwrap();
        assert!(matches!(
            detect_json_mcpservers(&path),
            (HarnessState::Malformed(_), None)
        ));
    }

    #[test]
    fn detect_json_mcpservers_wiring_is_scoped_to_the_ironmem_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("settings.json");
        // A sibling entry with --connect must not leak into ironmem's verdict.
        std::fs::write(
            &path,
            r#"{"mcpServers":{"other":{"command":"x","args":["serve","--connect","/tmp/o.sock"]},"ironmem":{"command":"ironmem","args":["serve"]}}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_json_mcpservers(&path),
            (HarnessState::Registered, Some(false))
        );
        // Only the mcpServers key counts: Muse's mcp_servers alias is neither
        // read nor written by ironmem, so doctor and the launcher agree.
        std::fs::write(
            &path,
            r#"{"mcp_servers":{"ironmem":{"command":"ironmem"}}}"#,
        )
        .unwrap();
        assert_eq!(
            detect_json_mcpservers(&path),
            (HarnessState::NotRegistered, None)
        );
    }

    #[test]
    fn detect_codex_classifies_states_via_section_header() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        assert_eq!(detect_codex(&path), HarnessState::Absent);
        std::fs::write(&path, "[mcp_servers.ironmem]\ncommand = \"ironmem\"\n").unwrap();
        assert_eq!(detect_codex(&path), HarnessState::Registered);
        std::fs::write(&path, "[other]\nx = 1\n").unwrap();
        assert_eq!(detect_codex(&path), HarnessState::NotRegistered);
    }

    #[test]
    fn harness_muse_check_key_is_a_literal() {
        // The muse registry row must surface as harness_muse (literal key,
        // not Box::leak) with real detection behind it: drive a Registered
        // config through the joined path and require Ok, so deleting the
        // JSON detection arm (which would fall to the Info catch-all)
        // fails this test. XDG_CONFIG_HOME is injected, not set in the
        // process, so no sibling test can observe it.
        let dir = tempfile::tempdir().unwrap();
        let muse_dir = dir.path().join("muse");
        std::fs::create_dir_all(&muse_dir).unwrap();
        std::fs::write(
            muse_dir.join("settings.json"),
            r#"{"schema_version":1,"mcpServers":{"ironmem":{"command":"ironmem","args":["serve","--connect","/tmp/d.sock"]}}}"#,
        )
        .unwrap();
        let checks = harness_checks(None, Some(dir.path().as_os_str()), crate::harness::REGISTRY);
        let muse = checks
            .iter()
            .find(|c| c.name == "harness_muse")
            .expect("harness_muse check must exist");
        assert_eq!(
            muse.status,
            CheckStatus::Ok,
            "registered muse is Ok: {muse:?}"
        );
        assert!(
            muse.summary.contains("registered"),
            "summary must say registered: {muse:?}"
        );
        assert!(
            muse.summary.contains("proxy command"),
            "proxy wiring note must be present: {muse:?}"
        );
    }

    #[test]
    fn json_harnesses_not_registered_hint_points_at_their_launcher() {
        // scripts/install-ironmem.sh wires only Claude and Codex; the other
        // JSON harnesses must send the user to `ironmem <id>` instead.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".config").join("muse")).unwrap();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        for rel in [
            ".claude.json",
            ".config/muse/settings.json",
            ".gemini/settings.json",
            ".grok/settings.json",
        ] {
            std::fs::write(home.join(rel), r#"{"mcpServers":{}}"#).unwrap();
        }
        let checks = harness_checks(Some(home), None, crate::harness::REGISTRY);
        let hint = |name: &str| {
            checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("{name} must exist"))
                .hint
                .clone()
                .unwrap_or_default()
        };
        assert!(hint("harness_claude").contains("install-ironmem.sh"));
        for (name, id) in [
            ("harness_muse", "muse"),
            ("harness_gemini", "gemini"),
            ("harness_grok", "grok"),
        ] {
            let h = hint(name);
            assert!(h.contains(&format!("ironmem {id}")), "{name}: {h}");
            assert!(h.contains("covers only Claude and Codex"), "{name}: {h}");
        }
    }

    #[test]
    fn harness_checks_detect_grok_and_gemini_through_the_shared_table() {
        // The launcher writes ~/.gemini/settings.json and ~/.grok/settings.json
        // in Claude's shape; doctor must read them back from the same table.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".gemini")).unwrap();
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        std::fs::write(
            home.join(".gemini").join("settings.json"),
            r#"{"mcpServers":{"ironmem":{"command":"ironmem","args":["serve","--connect","/tmp/d.sock"]}}}"#,
        )
        .unwrap();
        std::fs::write(
            home.join(".grok").join("settings.json"),
            r#"{"mcpServers":{"ironmem":{"command":"ironmem","args":["serve"]}}}"#,
        )
        .unwrap();
        let checks = harness_checks(Some(home), None, crate::harness::REGISTRY);
        let by_name = |name: &str| {
            checks
                .iter()
                .find(|c| c.name == name)
                .unwrap_or_else(|| panic!("{name} must exist"))
        };
        let gemini = by_name("harness_gemini");
        assert_eq!(gemini.status, CheckStatus::Ok, "{gemini:?}");
        assert!(
            gemini
                .summary
                .contains("wired with the shared-daemon proxy command"),
            "{gemini:?}"
        );
        let grok = by_name("harness_grok");
        assert_eq!(grok.status, CheckStatus::Ok, "{grok:?}");
        assert!(
            grok.summary
                .contains("upgrade available via `ironmem grok`"),
            "{grok:?}"
        );
    }

    #[test]
    fn harness_check_maps_states_to_statuses() {
        let p = Path::new("/tmp/example-config");
        let cases = [
            (HarnessState::Absent, CheckStatus::Info),
            (HarnessState::Registered, CheckStatus::Ok),
            (HarnessState::NotRegistered, CheckStatus::Warn),
            (HarnessState::Unreadable("io".into()), CheckStatus::Warn),
            (HarnessState::Malformed("syntax".into()), CheckStatus::Warn),
        ];
        for (state, expected) in cases {
            let c = harness_check("harness_x", "X", p, state);
            assert_eq!(c.status, expected);
            // No harness state is ever a blocking setup failure.
            assert!(!c.status.is_blocking());
        }
    }

    #[test]
    fn harness_checks_third_harness_yields_harness_id_key() {
        use crate::harness::{HarnessSpec, TranscriptParserKind};

        // A synthetic id no arm knows: exercises the Box::leak key path and
        // the "detection not yet implemented" Info catch-all (gemini used to
        // play this role before it got real detection).
        const ZETA_SPEC: HarnessSpec = HarnessSpec {
            id: "zeta",
            display_name: "Zeta",
            binary: "zeta",
            rules_file: "ZETA.md",
            rules_strategy: crate::harness::RulesStrategy::Import {
                directive: "@./AGENTS.md",
            },
            write_rules_default: false,
            client_info_aliases: &["zeta"],
            env_aliases: &["zeta"],
            additional_context_support: false,
            occupancy_support: false,
            transcript_parser: TranscriptParserKind::None,
        };

        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let injected = [
            crate::harness::REGISTRY[0],
            crate::harness::REGISTRY[1],
            ZETA_SPEC,
        ];

        let checks = harness_checks(Some(home), None, &injected);

        assert_eq!(checks.len(), 3, "one check per registry entry");
        assert_eq!(checks[0].name, "harness_claude", "first is claude");
        assert_eq!(checks[1].name, "harness_codex", "second is codex");
        // Box::leak path — verify the exact string value for the third harness.
        assert_eq!(
            checks[2].name, "harness_zeta",
            "third is zeta via Box::leak"
        );
        assert_eq!(
            checks[2].status,
            CheckStatus::Info,
            "unimplemented detection is Info"
        );
        assert!(
            checks[2].summary.contains("not yet implemented"),
            "{:?}",
            checks[2]
        );
    }

    #[test]
    fn run_doctor_emits_the_expected_check_keys_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            db_path: dir.path().join("m.sqlite3"),
            model_dir: dir.path().join("models"),
            model_dir_explicit: true,
            state_dir: dir.path().join("state"),
            mcp_access_mode: McpAccessMode::ReadOnly,
            embed_mode: EmbedMode::Noop,
        };
        let report = run_doctor(&cfg);
        let names: Vec<&str> = report.checks.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "binary",
                "database",
                "model",
                "mcp_access",
                "warmup",
                "harness_claude",
                "harness_codex",
                // grok + gemini (#190 Task 11) and muse all resolve through
                // the shared JSON-mcpServers detector (object shape).
                "harness_grok",
                "harness_gemini",
                "harness_muse",
            ],
            "the toolable check-key set must stay stable"
        );
    }

    // ---- #190 Task 14: proxy-wiring detection ------------------------------

    #[test]
    fn codex_proxy_wiring_detects_connect_arg() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[mcp_servers.ironmem]\ncommand = \"ironmem\"\nargs = [\"serve\", \"--connect\", \"/tmp/d.sock\"]\n",
        )
        .unwrap();
        assert_eq!(codex_proxy_wiring(&path), Some(true));
    }

    #[test]
    fn codex_proxy_wiring_detects_bare_serve() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[mcp_servers.ironmem]\ncommand = \"ironmem\"\nargs = [\"serve\"]\n",
        )
        .unwrap();
        assert_eq!(codex_proxy_wiring(&path), Some(false));
    }

    /// A `--connect` mention in some OTHER section must never produce a false
    /// positive for the `ironmem` section specifically.
    #[test]
    fn codex_proxy_wiring_scoped_to_ironmem_section_only() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[mcp_servers.other]\nargs = [\"--connect\", \"unrelated\"]\n\n[mcp_servers.ironmem]\ncommand = \"ironmem\"\nargs = [\"serve\"]\n",
        )
        .unwrap();
        assert_eq!(
            codex_proxy_wiring(&path),
            Some(false),
            "the ironmem section itself has no --connect; the other section's must not leak in"
        );
    }

    #[test]
    fn harness_checks_notes_proxy_wiring_for_registered_claude() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(
            home.join(".claude.json"),
            r#"{"mcpServers":{"ironmem":{"command":"ironmem","args":["serve","--connect","/tmp/d.sock"]}}}"#,
        )
        .unwrap();

        let checks = harness_checks(Some(home), None, crate::harness::REGISTRY);
        let claude = checks
            .iter()
            .find(|c| c.name == "harness_claude")
            .expect("harness_claude check must exist");
        assert!(
            claude
                .summary
                .contains("wired with the shared-daemon proxy command"),
            "got: {}",
            claude.summary
        );
    }

    #[test]
    fn harness_checks_flags_legacy_bare_serve_for_registered_codex() {
        let dir = tempfile::tempdir().unwrap();
        // Set CODEX_HOME explicitly (rather than clearing it) so this test
        // is deterministic regardless of the ambient environment — no other
        // test in this crate touches CODEX_HOME, so this doesn't race.
        let codex_home = dir.path().join("codex-home");
        std::fs::create_dir_all(&codex_home).unwrap();
        std::env::set_var("CODEX_HOME", &codex_home);
        std::fs::write(
            codex_home.join("config.toml"),
            "[mcp_servers.ironmem]\ncommand = \"ironmem\"\nargs = [\"serve\"]\n",
        )
        .unwrap();

        let checks = harness_checks(Some(dir.path()), None, crate::harness::REGISTRY);
        let codex = checks
            .iter()
            .find(|c| c.name == "harness_codex")
            .expect("harness_codex check must exist");
        assert!(
            codex.summary.contains("legacy bare `serve` command"),
            "got: {}",
            codex.summary
        );
        std::env::remove_var("CODEX_HOME");
    }

    #[test]
    fn harness_checks_no_wiring_note_when_not_registered() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        // No .claude.json at all -> Absent, not Registered.
        let checks = harness_checks(Some(home), None, crate::harness::REGISTRY);
        let claude = checks
            .iter()
            .find(|c| c.name == "harness_claude")
            .expect("harness_claude check must exist");
        assert!(!claude.summary.contains("wired with"));
    }

    // ---- #190 Task 14: run_doctor_with_daemon ------------------------------

    fn test_doctor_config(dir: &Path) -> Config {
        Config {
            db_path: dir.join("m.sqlite3"),
            model_dir: dir.join("models"),
            model_dir_explicit: true,
            state_dir: dir.join("state"),
            mcp_access_mode: McpAccessMode::ReadOnly,
            embed_mode: EmbedMode::Noop,
        }
    }

    #[tokio::test]
    async fn run_doctor_with_daemon_reports_unreachable_when_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_doctor_config(dir.path());

        let report = run_doctor_with_daemon(&cfg).await;
        let daemon = report
            .checks
            .iter()
            .find(|c| c.name == "daemon")
            .expect("daemon check must exist");
        assert_eq!(daemon.status, CheckStatus::Info);
        assert!(daemon.summary.contains("no shared daemon running"));
        assert!(
            !daemon.status.is_blocking(),
            "no daemon running is advisory, not blocking"
        );
    }

    /// #190 Task 14 acceptance: probing a running daemon reports reachable
    /// AND does not disturb its recorded attribution — this daemon-level
    /// guarantee is proven directly in `mcp::daemon`'s
    /// `health_probe_does_not_disturb_another_connections_attribution`; here
    /// we confirm `doctor`'s own wiring reports `Ok`/"reachable" through the
    /// full `run_doctor_with_daemon` seam.
    #[tokio::test]
    async fn run_doctor_with_daemon_reports_reachable_for_running_daemon() {
        use crate::mcp::app::App;
        use crate::mcp::daemon::{bind_daemon_listener, serve_accept_loop};
        use std::sync::Arc;
        use tokio::sync::oneshot;

        let dir = tempfile::tempdir().unwrap();
        let cfg = test_doctor_config(dir.path());
        // Default derivation: `<state_dir>/daemon.sock` — no env override
        // needed (and none used, so this can't race config.rs's own
        // IRONMEM_DAEMON_SOCKET-mutating tests, which guard that var with a
        // dedicated lock this module doesn't share).
        let socket_path = cfg.daemon_socket_path();
        let socket_path_thread = socket_path.clone();

        // `Arc<App>` is `!Send` (App is `!Sync`), so the daemon MUST run on
        // its own dedicated thread with its own runtime — never
        // `tokio::spawn`ed onto this test's own (multi-thread) runtime. This
        // mirrors every daemon-binding test in `mcp::daemon`.
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let daemon = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&socket_path_thread).await.unwrap();
                serve_accept_loop(
                    app,
                    listener,
                    std::time::Duration::from_secs(600),
                    shutdown_rx,
                )
                .await
                .unwrap();
            });
        });
        for _ in 0..200 {
            if socket_path.exists() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let report = run_doctor_with_daemon(&cfg).await;
        let daemon_check = report
            .checks
            .iter()
            .find(|c| c.name == "daemon")
            .expect("daemon check must exist");
        assert_eq!(daemon_check.status, CheckStatus::Ok);
        assert!(daemon_check.summary.contains("reachable"));

        shutdown_tx.send(()).ok();
        tokio::task::spawn_blocking(move || daemon.join().unwrap())
            .await
            .unwrap();
    }

    /// #321 review finding: a daemon that answered but rejected the
    /// handshake (alive, running incompatible/stale code) must be reported
    /// distinctly from both `Reachable` and `Unreachable` — a `Warn`, not an
    /// `Info`, and without the auto-spawn hint (auto-spawn only helps when
    /// nothing is listening; it does nothing for a stale running daemon).
    /// `daemon_check` is a pure function of `DaemonHealth`, so this is
    /// exercised directly rather than through a real daemon — the daemon-side
    /// classification into `RejectedHandshake` is proven end-to-end by
    /// `mcp::daemon::daemon_tests::probe_classifies_a_rejected_handshake_against_a_running_daemon`.
    #[test]
    fn daemon_check_reports_warn_for_a_rejected_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let socket_path = dir.path().join("daemon.sock");

        let check = daemon_check(
            &socket_path,
            crate::mcp::daemon::DaemonHealth::RejectedHandshake,
        );
        assert_eq!(check.status, CheckStatus::Warn);
        assert!(
            check
                .summary
                .contains("rejected the health-probe handshake"),
            "got: {check:?}"
        );
        let hint = check
            .hint
            .as_ref()
            .expect("a rejected handshake must carry a remediation hint");
        assert!(
            hint.contains("restarting"),
            "hint should point at restarting the stale daemon, got: {hint}"
        );
        assert!(
            !hint.contains("auto-spawn"),
            "auto-spawn only helps when nothing is listening — it must not be \
             suggested for a daemon that IS running but rejected the \
             handshake, got: {hint}"
        );
    }

    #[test]
    fn autospawn_check_reports_enabled_and_disabled() {
        std::env::remove_var("IRONMEM_NO_DAEMON");
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_doctor_config(dir.path());
        assert_eq!(autospawn_check(&cfg).status, CheckStatus::Info);
        assert!(autospawn_check(&cfg).summary.contains("enabled"));

        std::env::set_var("IRONMEM_NO_DAEMON", "1");
        assert!(autospawn_check(&cfg).summary.contains("disabled"));
        std::env::remove_var("IRONMEM_NO_DAEMON");
    }

    /// #190 Task 14 acceptance: doctor JSON includes the new daemon +
    /// per-harness wired keys, appended after the existing stable key set, in
    /// a stable order.
    #[tokio::test]
    async fn run_doctor_with_daemon_includes_new_keys_in_stable_order() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_doctor_config(dir.path());

        let report = run_doctor_with_daemon(&cfg).await;
        let names: Vec<&str> = report.checks.iter().map(|c| c.name).collect();
        assert_eq!(
            names,
            vec![
                "binary",
                "database",
                "model",
                "mcp_access",
                "warmup",
                "harness_claude",
                "harness_codex",
                "harness_grok",
                "harness_gemini",
                "harness_muse",
                "daemon",
                "daemon_autospawn",
            ]
        );

        // Also confirm the JSON serialization round-trips these keys.
        let json = serde_json::to_string(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        let json_names: Vec<&str> = v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|c| c["name"].as_str())
            .collect();
        assert_eq!(json_names, names);
    }
}
