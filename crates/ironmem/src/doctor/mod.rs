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
    checks.push(agy_hooks_check(home.as_deref()));
    checks.push(gemini_cli_legacy_check(home.as_deref()));
    note_agy_installer_overrides(
        &mut checks,
        home.as_deref(),
        std::env::var_os("AGY_CONFIG_JSON").as_deref(),
        std::env::var_os("AGY_HOOKS_JSON").as_deref(),
    );

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
/// - `"claude"`, `"agy"`, `"grok"`, `"muse"` → one shared JSON parse
///   ([`detect_json_mcpservers`]) of the file
///   [`crate::launcher::json_mcpservers_config_path_for`] resolves for the
///   id — the same table the launcher writes through — matching the
///   `mcpServers` OBJECT for an `"ironmem"` entry. Muse's object shape is
///   proven live on Muse Code 1.0.2 (`muse exec` completes `initialize` +
///   `tools/list` on it, while an array-shaped `mcpServers` is silently
///   ignored); Antigravity's (`~/.gemini/config/mcp_config.json`) is verified
///   live via `agy mcp add`/`agy mcp list`; Grok's is best-effort (see the
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
                "agy" => "harness_agy",
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
                "claude" | "agy" | "grok" | "muse" => {
                    let path = crate::launcher::json_mcpservers_config_path_for(
                        spec.id,
                        home.map(Path::to_path_buf),
                        xdg_config_home.map(OsStr::to_os_string),
                    );
                    match path {
                        Some(path) => {
                            let (state, wired) = detect_json_mcpservers(&path);
                            let check = harness_check(key, spec.display_name, &path, state.clone());
                            let check = if state == HarnessState::NotRegistered {
                                check.with_hint(registration_hint(spec, registry))
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

/// The hint for a JSON-config harness whose config does not register
/// ironmem, from the registry's `installer_registers`. A harness the
/// installer covers is pointed at the installer first, since it also
/// refreshes the binary; its launcher is the second way back. Any other
/// harness's launcher is its only route, and the hint names the harnesses the
/// installer does cover.
fn registration_hint(
    spec: &crate::harness::HarnessSpec,
    registry: &[crate::harness::HarnessSpec],
) -> String {
    if spec.installer_registers {
        return format!(
            "re-run scripts/install-ironmem.sh to register it (`ironmem {}` also registers, but needs the {} binary on PATH and then launches a session)",
            spec.id, spec.binary
        );
    }
    let covered: Vec<&str> = registry
        .iter()
        .filter(|s| s.installer_registers)
        .map(|s| s.display_name)
        .collect();
    let covered = match covered.split_last() {
        Some((last, [])) => last.to_string(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => "no harness".to_string(),
    };
    format!(
        "run `ironmem {}` to register it (scripts/install-ironmem.sh covers {covered})",
        spec.id
    )
}

/// The lifecycle hooks `scripts/install-ironmem.sh` writes into the `ironmem`
/// group of Antigravity's global `hooks.json`, as `(agy event, ironmem
/// hook)` pairs. The installer is the only source of agy hooks: the
/// `.agy-plugin` ships its MCP server alone.
const AGY_HOOK_EVENTS: &[(&str, &str)] = &[
    ("SessionStart", "session-start"),
    ("PreInvocation", "user-prompt-submit"),
    ("Stop", "stop"),
];

/// Whether `command` runs `ironmem hook <hook> --harness agy`, matched on
/// whitespace-separated words so the installer's mode prefix and quoting of
/// the binary path don't matter.
fn runs_agy_hook(command: &str, hook: &str) -> bool {
    let words: Vec<&str> = command.split_whitespace().collect();
    words.windows(2).any(|w| w == ["hook", hook])
        && words.windows(2).any(|w| w == ["--harness", "agy"])
}

/// Check that Antigravity's global `~/.gemini/config/hooks.json` carries the
/// `ironmem` hook group with all three lifecycle hooks, enabled. Without it
/// agy still reaches the MCP server (`harness_agy`) but gets no session
/// context, prompt recall or Stop-time mining, with no other sign of it.
fn agy_hooks_check(home: Option<&Path>) -> Check {
    const KEY: &str = "agy_hooks";
    const HINT: &str = "re-run scripts/install-ironmem.sh with agy on PATH; it writes the `ironmem` SessionStart, PreInvocation and Stop hooks";
    let Some(home) = home else {
        return Check::new(
            KEY,
            CheckStatus::Info,
            "Antigravity hooks: home directory unknown, skipped",
        );
    };
    let path = home.join(".gemini").join("config").join("hooks.json");
    let shown = path.display();
    let warn = |summary: String| Check::new(KEY, CheckStatus::Warn, summary).with_hint(HINT);
    let raw = match read_harness_config(&path) {
        Ok(raw) => raw,
        Err(HarnessState::Absent) => {
            return Check::new(
                KEY,
                CheckStatus::Info,
                format!("Antigravity hooks: no hooks file at {shown}"),
            )
            .with_hint(HINT)
        }
        Err(HarnessState::Unreadable(e)) => {
            return warn(format!(
                "Antigravity hooks: {shown} present but unreadable: {e}"
            ))
        }
        Err(other) => {
            return warn(format!(
                "Antigravity hooks: {shown} could not be read ({other:?})"
            ))
        }
    };
    // agy creates its config files empty; that is "no group", not a parse error.
    let root: serde_json::Value = if raw.trim().is_empty() {
        serde_json::json!({})
    } else {
        match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                return warn(format!(
                    "Antigravity hooks: {shown} could not be parsed: {e}"
                ))
            }
        }
    };
    let Some(group) = root.get("ironmem") else {
        return warn(format!(
            "Antigravity hooks: no 'ironmem' hook group in {shown}"
        ));
    };
    let Some(group) = group.as_object() else {
        return warn(format!(
            "Antigravity hooks: 'ironmem' in {shown} is not a hook group object"
        ));
    };
    if group.get("enabled").and_then(serde_json::Value::as_bool) == Some(false) {
        return Check::new(
            KEY,
            CheckStatus::Warn,
            format!("Antigravity hooks: the 'ironmem' hook group in {shown} is disabled"),
        )
        .with_hint(
            "remove `\"enabled\": false` from the 'ironmem' group to turn the hooks back on",
        );
    }
    let missing: Vec<&str> = AGY_HOOK_EVENTS
        .iter()
        .filter(|(event, hook)| {
            let wired = group
                .get(*event)
                .and_then(serde_json::Value::as_array)
                .is_some_and(|handlers| {
                    handlers.iter().any(|handler| {
                        handler
                            .get("command")
                            .and_then(serde_json::Value::as_str)
                            .is_some_and(|cmd| runs_agy_hook(cmd, hook))
                    })
                });
            !wired
        })
        .map(|(event, _)| *event)
        .collect();
    if missing.is_empty() {
        Check::new(
            KEY,
            CheckStatus::Ok,
            format!("Antigravity hooks: SessionStart, PreInvocation and Stop wired in {shown}"),
        )
    } else {
        warn(format!(
            "Antigravity hooks: the 'ironmem' group in {shown} does not run ironmem's {} hook{}",
            missing.join(", "),
            if missing.len() == 1 { "" } else { "s" }
        ))
    }
}

/// Add a hint to `harness_agy` / `agy_hooks` when the installer's
/// `AGY_CONFIG_JSON` / `AGY_HOOKS_JSON` override is set in doctor's
/// environment and points away from the file agy reads.
///
/// Those variables only choose where `scripts/install-ironmem.sh` WRITES.
/// agy reads its global config from `~/.gemini/config/` (its embedded docs
/// name `~/.gemini/config/mcp_config.json` as the global MCP file), and no
/// agy setting moving that directory is known, so doctor keeps checking
/// agy's own paths and says why a file written elsewhere does not count.
fn note_agy_installer_overrides(
    checks: &mut [Check],
    home: Option<&Path>,
    config_override: Option<&OsStr>,
    hooks_override: Option<&OsStr>,
) {
    let Some(home) = home else { return };
    let agy_dir = home.join(".gemini").join("config");
    for (check_name, var, value, file) in [
        (
            "harness_agy",
            "AGY_CONFIG_JSON",
            config_override,
            "mcp_config.json",
        ),
        ("agy_hooks", "AGY_HOOKS_JSON", hooks_override, "hooks.json"),
    ] {
        let Some(value) = value.filter(|v| !v.is_empty()) else {
            continue;
        };
        let read = agy_dir.join(file);
        let set = Path::new(value);
        let same = |a: &Path, b: &Path| {
            a == b
                || matches!(
                    (std::fs::canonicalize(a), std::fs::canonicalize(b)),
                    (Ok(a), Ok(b)) if a == b
                )
        };
        if same(set, &read) {
            continue;
        }
        let note = format!(
            "{var} is set to {}: the installer writes there, but agy reads only {}",
            set.display(),
            read.display()
        );
        if let Some(check) = checks.iter_mut().find(|c| c.name == check_name) {
            check.hint = Some(match check.hint.take() {
                Some(hint) => format!("{hint}; {note}"),
                None => note,
            });
        }
    }
}

/// Flag the `mcpServers.ironmem` entry the old `ironmem gemini` launcher
/// wrote into Gemini CLI's `~/.gemini/settings.json`. Gemini CLI is
/// deprecated upstream in favour of Antigravity, and ironmem no longer
/// registers or upgrades that entry, so a leftover one is stale wiring the
/// user should move to `ironmem agy`.
fn gemini_cli_legacy_check(home: Option<&Path>) -> Check {
    const KEY: &str = "gemini_cli_legacy";
    let Some(home) = home else {
        return Check::new(
            KEY,
            CheckStatus::Info,
            "Gemini CLI (deprecated): home directory unknown, skipped",
        );
    };
    let path = home.join(".gemini").join("settings.json");
    let shown = path.display();
    match detect_json_mcpservers(&path).0 {
        HarnessState::Registered => Check::new(
            KEY,
            CheckStatus::Warn,
            format!(
                "Gemini CLI (deprecated): leftover ironmem MCP entry in {shown}, written by the old `ironmem gemini` launcher"
            ),
        )
        .with_hint(format!(
            "Gemini CLI was replaced by Antigravity: run `ironmem agy` (or scripts/install-ironmem.sh) to register ironmem for agy, then delete `mcpServers.ironmem` from {shown}"
        )),
        HarnessState::Absent | HarnessState::NotRegistered => Check::new(
            KEY,
            CheckStatus::Info,
            "Gemini CLI (deprecated): no leftover ironmem registration",
        ),
        HarnessState::Unreadable(e) | HarnessState::Malformed(e) => Check::new(
            KEY,
            CheckStatus::Info,
            format!("Gemini CLI (deprecated): could not check {shown} for a leftover ironmem entry: {e}"),
        ),
    }
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
/// (`~/.claude.json`, Antigravity `~/.gemini/config/mcp_config.json`, Grok
/// `settings.json`, Muse `settings.json`)
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
    // Antigravity creates `mcp_config.json` as a 0-byte file on first run,
    // and the launcher registers into an empty or blank file as if it were
    // missing; a config with nothing in it registers nothing.
    if raw.trim().is_empty() {
        return (HarnessState::NotRegistered, None);
    }
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
        // An empty or blank file (agy creates mcp_config.json at 0 bytes) is
        // "not registered", not malformed.
        for blank in ["", " \n\t\n"] {
            std::fs::write(&path, blank).unwrap();
            assert_eq!(
                detect_json_mcpservers(&path),
                (HarnessState::NotRegistered, None),
                "{blank:?}"
            );
        }
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
        // scripts/install-ironmem.sh wires Claude, Codex, Muse and Antigravity
        // (`installer_registers`); the JSON harness it does NOT cover (Grok)
        // must send the user to `ironmem grok` instead.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".config").join("muse")).unwrap();
        std::fs::create_dir_all(home.join(".gemini").join("config")).unwrap();
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        for rel in [
            ".claude.json",
            ".config/muse/settings.json",
            ".gemini/config/mcp_config.json",
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
        // Claude, Muse and Antigravity are script-covered
        // (`installer_registers`), so their hints lead with the script rather
        // than sending the user to a launcher the script already covers.
        for (name, id) in [
            ("harness_claude", "claude"),
            ("harness_muse", "muse"),
            ("harness_agy", "agy"),
        ] {
            let h = hint(name);
            assert!(h.contains("install-ironmem.sh"), "{name}: {h}");
            assert!(h.contains(&format!("ironmem {id}")), "{name}: {h}");
            assert!(!h.contains("covers"), "{name}: {h}");
        }
        let grok = hint("harness_grok");
        assert!(grok.contains("ironmem grok"), "harness_grok: {grok}");
        // The covered list is the registry's, in registry order.
        assert!(
            grok.contains("covers Claude Code, Codex, Antigravity and Muse Code"),
            "harness_grok: {grok}"
        );
    }

    /// The hint follows the spec's `installer_registers`, not its id: the
    /// same Grok row flagged as installer-covered gets the installer hint.
    #[test]
    fn registration_hint_is_driven_by_installer_registers() {
        let grok = *crate::harness::by_id("grok", crate::harness::REGISTRY).unwrap();
        let covered = crate::harness::HarnessSpec {
            installer_registers: true,
            ..grok
        };
        let h = registration_hint(&covered, crate::harness::REGISTRY);
        assert!(h.starts_with("re-run scripts/install-ironmem.sh"), "{h}");
        assert!(h.contains("`ironmem grok`"), "{h}");

        let only = [grok, covered];
        let h = registration_hint(&grok, &only);
        assert!(h.ends_with("covers Grok)"), "single covered harness: {h}");
        let h = registration_hint(&grok, &[grok]);
        assert!(h.ends_with("covers no harness)"), "{h}");
    }

    /// The 0-byte `mcp_config.json` agy creates on first run reports as
    /// "not registered", with the installer hint, not as a malformed config.
    #[test]
    fn harness_agy_reports_the_empty_file_agy_creates_as_not_registered() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let config_dir = home.join(".gemini").join("config");
        std::fs::create_dir_all(&config_dir).unwrap();
        std::fs::write(config_dir.join("mcp_config.json"), "").unwrap();
        let checks = harness_checks(Some(home), None, crate::harness::REGISTRY);
        let agy = checks.iter().find(|c| c.name == "harness_agy").unwrap();
        assert_eq!(agy.status, CheckStatus::Warn, "{agy:?}");
        assert!(agy.summary.contains("not registered"), "{agy:?}");
        assert!(!agy.summary.contains("parse"), "{agy:?}");
        assert!(
            agy.hint
                .as_deref()
                .unwrap_or("")
                .contains("install-ironmem.sh"),
            "{agy:?}"
        );
    }

    #[test]
    fn harness_checks_detect_grok_and_agy_through_the_shared_table() {
        // The launcher writes ~/.gemini/config/mcp_config.json and
        // ~/.grok/settings.json in Claude's shape; doctor must read them back
        // from the same table.
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::create_dir_all(home.join(".gemini").join("config")).unwrap();
        std::fs::create_dir_all(home.join(".grok")).unwrap();
        std::fs::write(
            home.join(".gemini").join("config").join("mcp_config.json"),
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
        let agy = by_name("harness_agy");
        assert_eq!(agy.status, CheckStatus::Ok, "{agy:?}");
        assert!(
            agy.summary
                .contains("wired with the shared-daemon proxy command"),
            "{agy:?}"
        );
        let grok = by_name("harness_grok");
        assert_eq!(grok.status, CheckStatus::Ok, "{grok:?}");
        assert!(
            grok.summary
                .contains("upgrade available via `ironmem grok`"),
            "{grok:?}"
        );
    }

    /// The `ironmem` group `scripts/install-ironmem.sh` writes, with the
    /// mode prefix and a quoted binary path it really uses.
    fn installer_agy_hooks() -> serde_json::Value {
        let cmd = |hook: &str| {
            format!(
                "IRONMEM_MCP_MODE=\"${{IRONMEM_MCP_MODE:-trusted}}\" '/home/u/.ironrace/bin/ironmem' hook {hook} --harness agy"
            )
        };
        serde_json::json!({
            "other-plugin": { "Stop": [{ "type": "command", "command": "true" }] },
            "ironmem": {
                "SessionStart": [{ "type": "command", "command": cmd("session-start"), "timeout": 60 }],
                "PreInvocation": [{ "type": "command", "command": cmd("user-prompt-submit") }],
                "Stop": [{ "type": "command", "command": cmd("stop"), "timeout": 120 }]
            }
        })
    }

    #[test]
    fn agy_hooks_check_reports_each_wiring_state() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let config_dir = home.join(".gemini").join("config");
        let hooks = config_dir.join("hooks.json");
        let check = || agy_hooks_check(Some(home));

        let absent = check();
        assert_eq!(absent.name, "agy_hooks");
        assert_eq!(absent.status, CheckStatus::Info, "{absent:?}");

        std::fs::create_dir_all(&config_dir).unwrap();
        let write = |v: &serde_json::Value| std::fs::write(&hooks, v.to_string()).unwrap();

        std::fs::write(&hooks, "").unwrap();
        let empty = check();
        assert_eq!(empty.status, CheckStatus::Warn, "{empty:?}");
        assert!(
            empty.summary.contains("no 'ironmem' hook group"),
            "{empty:?}"
        );
        assert!(empty
            .hint
            .as_deref()
            .unwrap_or("")
            .contains("install-ironmem.sh"));

        write(&installer_agy_hooks());
        let ok = check();
        assert_eq!(ok.status, CheckStatus::Ok, "{ok:?}");

        let mut disabled = installer_agy_hooks();
        disabled["ironmem"]["enabled"] = serde_json::json!(false);
        write(&disabled);
        let off = check();
        assert_eq!(off.status, CheckStatus::Warn, "{off:?}");
        assert!(off.summary.contains("disabled"), "{off:?}");

        let mut partial = installer_agy_hooks();
        partial["ironmem"].as_object_mut().unwrap().remove("Stop");
        partial["ironmem"]["PreInvocation"][0]["command"] =
            serde_json::json!("ironmem hook user-prompt-submit --harness claude-code");
        write(&partial);
        let gaps = check();
        assert_eq!(gaps.status, CheckStatus::Warn, "{gaps:?}");
        assert!(
            gaps.summary.contains("PreInvocation, Stop hooks"),
            "{gaps:?}"
        );

        std::fs::write(&hooks, "{not json").unwrap();
        assert_eq!(check().status, CheckStatus::Warn);

        assert_eq!(agy_hooks_check(None).status, CheckStatus::Info);
    }

    /// An installer override pointing away from agy's own file is named in
    /// the hint, and the check still reports the file agy reads. An override
    /// naming agy's own path, or none at all, adds nothing.
    #[test]
    fn agy_checks_explain_an_installer_override_agy_does_not_read() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let elsewhere = dir.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        // The installer wrote a complete group, but to the override path.
        std::fs::write(
            elsewhere.join("hooks.json"),
            installer_agy_hooks().to_string(),
        )
        .unwrap();
        let fresh = || {
            vec![
                harness_checks(Some(&home), None, crate::harness::REGISTRY)
                    .into_iter()
                    .find(|c| c.name == "harness_agy")
                    .unwrap(),
                agy_hooks_check(Some(&home)),
            ]
        };
        let by =
            |checks: &[Check], name: &str| checks.iter().find(|c| c.name == name).unwrap().clone();

        let mut checks = fresh();
        note_agy_installer_overrides(
            &mut checks,
            Some(&home),
            Some(elsewhere.join("mcp_config.json").as_os_str()),
            Some(elsewhere.join("hooks.json").as_os_str()),
        );
        let hooks = by(&checks, "agy_hooks");
        assert_eq!(
            hooks.status,
            CheckStatus::Info,
            "still agy's own (absent) file: {hooks:?}"
        );
        let hint = hooks.hint.unwrap_or_default();
        assert!(hint.contains("AGY_HOOKS_JSON is set to"), "{hint}");
        assert!(
            hint.contains(&format!(
                "agy reads only {}",
                home.join(".gemini/config/hooks.json").display()
            )),
            "{hint}"
        );
        assert!(
            hint.contains("install-ironmem.sh"),
            "keeps the original hint: {hint}"
        );
        let mcp = by(&checks, "harness_agy").hint.unwrap_or_default();
        assert!(mcp.contains("AGY_CONFIG_JSON is set to"), "{mcp}");
        assert!(mcp.contains("mcp_config.json"), "{mcp}");

        for (config, hooks) in [
            (None, None),
            (
                Some(home.join(".gemini/config/mcp_config.json")),
                Some(home.join(".gemini/config/hooks.json")),
            ),
        ] {
            let mut checks = fresh();
            let before: Vec<Option<String>> = checks.iter().map(|c| c.hint.clone()).collect();
            note_agy_installer_overrides(
                &mut checks,
                Some(&home),
                config.as_deref().map(Path::as_os_str),
                hooks.as_deref().map(Path::as_os_str),
            );
            let after: Vec<Option<String>> = checks.iter().map(|c| c.hint.clone()).collect();
            assert_eq!(before, after, "{config:?} {hooks:?}");
        }
    }

    #[test]
    fn runs_agy_hook_matches_words_not_substrings() {
        assert!(runs_agy_hook("x hook stop --harness agy", "stop"));
        assert!(!runs_agy_hook("x hook stop-all --harness agy", "stop"));
        assert!(!runs_agy_hook("x hook stop --harness agy-old", "stop"));
        assert!(!runs_agy_hook("x hook stop --harness gemini", "stop"));
    }

    /// The old `ironmem gemini` launcher's `mcpServers.ironmem` entry in
    /// `~/.gemini/settings.json` is flagged with a hint to move to agy.
    #[test]
    fn gemini_cli_legacy_check_flags_a_leftover_entry() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        let settings = home.join(".gemini").join("settings.json");

        let none = gemini_cli_legacy_check(Some(home));
        assert_eq!(none.name, "gemini_cli_legacy");
        assert_eq!(none.status, CheckStatus::Info, "{none:?}");

        std::fs::create_dir_all(settings.parent().unwrap()).unwrap();
        std::fs::write(&settings, r#"{"mcpServers":{"other":{"command":"x"}}}"#).unwrap();
        assert_eq!(
            gemini_cli_legacy_check(Some(home)).status,
            CheckStatus::Info
        );

        std::fs::write(
            &settings,
            r#"{"mcpServers":{"ironmem":{"command":"ironmem","args":["serve","--connect","/tmp/d.sock"]}}}"#,
        )
        .unwrap();
        let leftover = gemini_cli_legacy_check(Some(home));
        assert_eq!(leftover.status, CheckStatus::Warn, "{leftover:?}");
        assert!(!leftover.status.is_blocking());
        let hint = leftover.hint.unwrap_or_default();
        assert!(hint.contains("ironmem agy"), "{hint}");
        assert!(hint.contains("mcpServers.ironmem"), "{hint}");
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
            hook_output: crate::harness::HookOutputFormat::HookResponse,
            prompt_flag: None,
            deprecated_aliases: &[],
            plugin_ships_hooks: true,
            installer_registers: false,
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
                // grok, agy, and muse all resolve through
                // the shared JSON-mcpServers detector (object shape).
                "harness_grok",
                "harness_agy",
                "harness_muse",
                // agy's installer-wired hooks, then the deprecated Gemini
                // CLI leftover (`harness_gemini` was renamed `harness_agy`).
                "agy_hooks",
                "gemini_cli_legacy",
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
                "harness_agy",
                "harness_muse",
                "agy_hooks",
                "gemini_cli_legacy",
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
