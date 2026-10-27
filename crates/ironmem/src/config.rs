//! Configuration loading and environment-variable parsing for `ironmem`.

use std::path::PathBuf;
use std::time::Duration;

use crate::error::MemoryError;

/// Default idle window before an otherwise-unused shared daemon shuts itself
/// down. Overridable at runtime via `IRONMEM_DAEMON_IDLE_SECS`.
const DEFAULT_DAEMON_IDLE_SECS: u64 = 300;

/// Default fail-safe bound on how long a write-shaped MCP tool handler will
/// block waiting for background memory init (`ReadinessGate::wait_for_write`)
/// before giving up and returning `MemoryError::NotReady`. Overridable at
/// runtime via `IRONMEM_WRITE_READINESS_TIMEOUT_SECS`. 90s is generous enough
/// to cover a real first-run ONNX model download + init (which can take tens
/// of seconds) while still bounding how long a caller can be left hanging if
/// init never completes.
const DEFAULT_WRITE_READINESS_TIMEOUT_SECS: u64 = 90;
/// Upper bound on `IRONMEM_WRITE_READINESS_TIMEOUT_SECS` (1 day). Keeps an
/// operator typo from producing a duration whose derived `Instant` deadline is
/// unrepresentable — `Instant + Duration` panics on overflow, and in a daemon
/// that panic would take down every connected client, not just the caller.
const MAX_WRITE_READINESS_TIMEOUT_SECS: u64 = 24 * 60 * 60;

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

        // Legacy path retained after the ironrace-memory → ironmem rename so existing
        // users' databases and hook state are found without manual migration.
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

    /// Filesystem path of the shared-daemon Unix socket.
    ///
    /// Defaults to `<state_dir>/daemon.sock`. Honors the `IRONMEM_DAEMON_SOCKET`
    /// env override (used verbatim when set to a non-empty value), which lets
    /// tests point the socket at a tempdir for deterministic behavior.
    ///
    /// The socket's file permission mode is fixed (0600, applied by the daemon
    /// task when it binds) and is deliberately NOT configurable — no arbitrary
    /// socket-mode knob is exposed.
    pub fn daemon_socket_path(&self) -> PathBuf {
        match std::env::var("IRONMEM_DAEMON_SOCKET") {
            Ok(p) if !p.is_empty() => PathBuf::from(p),
            _ => self.state_dir.join("daemon.sock"),
        }
    }

    /// Filesystem path of the daemon single-flight lockfile.
    ///
    /// Derived by appending `.lock` to the socket path's filename, e.g.
    /// `/x/daemon.sock` -> `/x/daemon.sock.lock`. It derives from
    /// [`Config::daemon_socket_path`], so an `IRONMEM_DAEMON_SOCKET` override is
    /// reflected here too. Note `.lock` is *appended* rather than substituted:
    /// `with_extension("lock")` would replace `.sock` and yield the wrong
    /// `daemon.lock`.
    pub fn daemon_lock_path(&self) -> PathBuf {
        let mut name = self.daemon_socket_path().into_os_string();
        name.push(".lock");
        PathBuf::from(name)
    }

    /// Idle timeout after which an unused daemon shuts itself down.
    ///
    /// Defaults to 300s ([`DEFAULT_DAEMON_IDLE_SECS`]). Honors the
    /// `IRONMEM_DAEMON_IDLE_SECS` env override, parsed as `u64` seconds. A
    /// present-but-unparseable value falls back to the default rather than
    /// erroring (no new error surface is introduced) but logs a `tracing::warn!`
    /// (M8) so the mistake is diagnosable instead of silently ignored; an
    /// ABSENT var is the normal case and stays silent.
    pub fn daemon_idle_timeout(&self) -> Duration {
        let secs = match std::env::var("IRONMEM_DAEMON_IDLE_SECS") {
            Ok(raw) => match raw.trim().parse::<u64>() {
                Ok(secs) => secs,
                Err(e) => {
                    tracing::warn!(
                        "IRONMEM_DAEMON_IDLE_SECS={raw:?} is not a valid number of seconds \
                         ({e}); using the default of {DEFAULT_DAEMON_IDLE_SECS}s instead"
                    );
                    DEFAULT_DAEMON_IDLE_SECS
                }
            },
            Err(_) => DEFAULT_DAEMON_IDLE_SECS,
        };
        Duration::from_secs(secs)
    }

    /// Fail-safe bound on how long a write-shaped MCP tool waits for readiness
    /// during server warm-up before giving up.
    ///
    /// Consumed by `server::dispatch_request` via
    /// `ReadinessGate::wait_for_write_async` on the production path, and by
    /// `tools::call_tool` via the synchronous `wait_for_write` fallback. No
    /// tool handler blocks on it directly.
    ///
    /// Defaults to 90s ([`DEFAULT_WRITE_READINESS_TIMEOUT_SECS`]). Honors the
    /// `IRONMEM_WRITE_READINESS_TIMEOUT_SECS` env override, parsed as `u64`
    /// seconds. A present-but-unparseable value falls back to the default
    /// rather than erroring (no new error surface is introduced) but logs a
    /// `tracing::warn!` so the mistake is diagnosable instead of silently
    /// ignored; an ABSENT var is the normal case and stays silent. Read fresh
    /// on every call (not cached on `Config`), so tests can override it with
    /// `std::env::set_var` immediately before exercising a still-pending gate,
    /// without needing to reconstruct `Config`.
    ///
    /// Clamped to [`MAX_WRITE_READINESS_TIMEOUT_SECS`] (1 day). The value is a
    /// crash guard, not a schedule — anything beyond a day is already
    /// indistinguishable from "wait forever" — and clamping keeps the derived
    /// deadline representable for every downstream `Instant`/timer arithmetic
    /// instead of relying on each of them to defend itself.
    pub fn write_readiness_timeout(&self) -> Duration {
        let secs = match std::env::var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS") {
            Ok(raw) => match raw.trim().parse::<u64>() {
                Ok(secs) if secs > MAX_WRITE_READINESS_TIMEOUT_SECS => {
                    tracing::warn!(
                        "IRONMEM_WRITE_READINESS_TIMEOUT_SECS={secs} exceeds the maximum of \
                         {MAX_WRITE_READINESS_TIMEOUT_SECS}s; clamping to the maximum"
                    );
                    MAX_WRITE_READINESS_TIMEOUT_SECS
                }
                Ok(secs) => secs,
                Err(e) => {
                    tracing::warn!(
                        "IRONMEM_WRITE_READINESS_TIMEOUT_SECS={raw:?} is not a valid number of \
                         seconds ({e}); using the default of \
                         {DEFAULT_WRITE_READINESS_TIMEOUT_SECS}s instead"
                    );
                    DEFAULT_WRITE_READINESS_TIMEOUT_SECS
                }
            },
            Err(_) => DEFAULT_WRITE_READINESS_TIMEOUT_SECS,
        };
        Duration::from_secs(secs)
    }

    /// Whether `ironmem serve` may auto-spawn a shared daemon.
    ///
    /// Enabled by default. Auto-spawn is disabled when `IRONMEM_NO_DAEMON` is
    /// set to any value that is not empty, `"0"`, `"false"`, or `"no"`
    /// (case-insensitive, surrounding whitespace ignored). In other words:
    /// unset / empty / `0` / `false` / `no` keep auto-spawn ON; anything else
    /// (e.g. `1`, `true`, `yes`) turns it OFF.
    pub fn daemon_autospawn_enabled(&self) -> bool {
        match std::env::var("IRONMEM_NO_DAEMON") {
            Ok(raw) => matches!(
                raw.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false" | "no"
            ),
            Err(_) => true,
        }
    }
}

/// THE lock for tests that mutate a process-global env var read by [`Config`].
///
/// Crate-wide on purpose. Env vars are process-global, and `config::tests` and
/// `mcp::server::tests` live in the SAME test binary — two separate mutexes
/// each "guarding" `IRONMEM_WRITE_READINESS_TIMEOUT_SECS` exclude nothing from
/// each other, so a clamp test setting `u64::MAX` could make an unrelated
/// framing-loop test park for a day and fail with a bogus diagnosis. Anything
/// touching these vars must take THIS lock.
#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Sets an env var for the duration of a test and restores the prior value on
/// drop, holding [`ENV_LOCK`] for its whole lifetime.
///
/// Drop-based rather than `catch_unwind`-based so it is panic-safe *and* works
/// across `.await` points (an async test cannot be wrapped in `catch_unwind`).
/// A failing assertion can therefore never leak an override into a later test.
#[cfg(test)]
pub(crate) struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
    _lock: std::sync::MutexGuard<'static, ()>,
}

#[cfg(test)]
impl EnvGuard {
    pub(crate) fn set(key: &'static str, value: &str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self {
            key,
            previous,
            _lock: lock,
        }
    }

    /// Take the lock without changing anything — for tests that only READ an
    /// env-controlled value but must not race one that writes it.
    pub(crate) fn pin(key: &'static str) -> Self {
        let lock = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let previous = std::env::var(key).ok();
        Self {
            key,
            previous,
            _lock: lock,
        }
    }
}

#[cfg(test)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var(self.key, value),
            None => std::env::remove_var(self.key),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Build a `Config` with a known `state_dir` without touching the real home
    /// directory (so path derivation is deterministic under test).
    fn test_config(state_dir: &str) -> Config {
        let state_dir = PathBuf::from(state_dir);
        Config {
            db_path: state_dir.join("memory.sqlite3"),
            model_dir: state_dir.join("models"),
            model_dir_explicit: false,
            state_dir,
            mcp_access_mode: McpAccessMode::ReadOnly,
            embed_mode: EmbedMode::Noop,
        }
    }

    /// `Instant + Duration` panics on overflow, and this duration is derived
    /// from an operator-supplied env var. In a shared daemon that panic takes
    /// down every connected client over one bad value, so an absurd timeout
    /// has to clamp rather than propagate.
    #[test]
    fn write_readiness_timeout_clamps_an_absurd_override() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let config = test_config("/tmp/ironmem-clamp-test");

        std::env::set_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS", u64::MAX.to_string());
        let clamped = config.write_readiness_timeout();
        assert_eq!(
            clamped,
            Duration::from_secs(MAX_WRITE_READINESS_TIMEOUT_SECS),
            "an out-of-range override must clamp to the maximum"
        );
        // The point of clamping: the derived deadline stays representable.
        assert!(
            std::time::Instant::now().checked_add(clamped).is_some(),
            "a clamped timeout must still produce a representable deadline"
        );

        // A sane override is still honored untouched.
        std::env::set_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS", "5");
        assert_eq!(config.write_readiness_timeout(), Duration::from_secs(5));

        std::env::remove_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS");
    }

    #[test]
    fn socket_path_defaults_to_state_dir() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_DAEMON_SOCKET");
        let cfg = test_config("/tmp/ironmem-test-state");
        assert_eq!(
            cfg.daemon_socket_path(),
            PathBuf::from("/tmp/ironmem-test-state/daemon.sock")
        );
    }

    #[test]
    fn lock_path_appends_dot_lock_to_socket() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_DAEMON_SOCKET");
        let cfg = test_config("/tmp/ironmem-test-state");
        // `.lock` is appended, not substituted: `.sock` is preserved.
        assert_eq!(
            cfg.daemon_lock_path(),
            PathBuf::from("/tmp/ironmem-test-state/daemon.sock.lock")
        );
    }

    #[test]
    fn socket_env_override_honored_for_socket_and_lock() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_DAEMON_SOCKET", "/run/custom/ironmem.sock");
        let cfg = test_config("/tmp/ironmem-test-state");
        assert_eq!(
            cfg.daemon_socket_path(),
            PathBuf::from("/run/custom/ironmem.sock")
        );
        // Lock path derives from the overridden socket path.
        assert_eq!(
            cfg.daemon_lock_path(),
            PathBuf::from("/run/custom/ironmem.sock.lock")
        );
        std::env::remove_var("IRONMEM_DAEMON_SOCKET");
    }

    #[test]
    fn empty_socket_override_falls_back_to_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_DAEMON_SOCKET", "");
        let cfg = test_config("/tmp/ironmem-test-state");
        assert_eq!(
            cfg.daemon_socket_path(),
            PathBuf::from("/tmp/ironmem-test-state/daemon.sock")
        );
        std::env::remove_var("IRONMEM_DAEMON_SOCKET");
    }

    #[test]
    fn idle_timeout_defaults_to_300s() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_DAEMON_IDLE_SECS");
        let cfg = test_config("/tmp/ironmem-test-state");
        assert_eq!(cfg.daemon_idle_timeout(), Duration::from_secs(300));
    }

    #[test]
    fn idle_timeout_env_override_parses() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_DAEMON_IDLE_SECS", "42");
        let cfg = test_config("/tmp/ironmem-test-state");
        assert_eq!(cfg.daemon_idle_timeout(), Duration::from_secs(42));
        std::env::remove_var("IRONMEM_DAEMON_IDLE_SECS");
    }

    #[test]
    fn idle_timeout_bad_value_falls_back_to_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_DAEMON_IDLE_SECS", "not-a-number");
        let cfg = test_config("/tmp/ironmem-test-state");
        // Bad value silently uses the default; no error surface introduced.
        assert_eq!(cfg.daemon_idle_timeout(), Duration::from_secs(300));
        std::env::remove_var("IRONMEM_DAEMON_IDLE_SECS");
    }

    #[test]
    fn write_readiness_timeout_defaults_to_90s() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS");
        let cfg = test_config("/tmp/ironmem-test-state");
        assert_eq!(cfg.write_readiness_timeout(), Duration::from_secs(90));
    }

    #[test]
    fn write_readiness_timeout_env_override_parses() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS", "5");
        let cfg = test_config("/tmp/ironmem-test-state");
        assert_eq!(cfg.write_readiness_timeout(), Duration::from_secs(5));
        std::env::remove_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS");
    }

    #[test]
    fn write_readiness_timeout_bad_value_falls_back_to_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS", "not-a-number");
        let cfg = test_config("/tmp/ironmem-test-state");
        // Bad value silently uses the default; no error surface introduced.
        assert_eq!(cfg.write_readiness_timeout(), Duration::from_secs(90));
        std::env::remove_var("IRONMEM_WRITE_READINESS_TIMEOUT_SECS");
    }

    #[test]
    fn autospawn_enabled_by_default() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        std::env::remove_var("IRONMEM_NO_DAEMON");
        let cfg = test_config("/tmp/ironmem-test-state");
        assert!(cfg.daemon_autospawn_enabled());
    }

    #[test]
    fn autospawn_disabled_when_no_daemon_truthy() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for truthy in ["1", "true", "yes", "on", "TRUE"] {
            std::env::set_var("IRONMEM_NO_DAEMON", truthy);
            let cfg = test_config("/tmp/ironmem-test-state");
            assert!(
                !cfg.daemon_autospawn_enabled(),
                "{truthy:?} should disable auto-spawn"
            );
        }
        std::env::remove_var("IRONMEM_NO_DAEMON");
    }

    #[test]
    fn autospawn_stays_enabled_for_falsey_values() {
        let _g = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        for falsey in ["", "0", "false", "no", "NO", "False"] {
            std::env::set_var("IRONMEM_NO_DAEMON", falsey);
            let cfg = test_config("/tmp/ironmem-test-state");
            assert!(
                cfg.daemon_autospawn_enabled(),
                "{falsey:?} should keep auto-spawn enabled"
            );
        }
        std::env::remove_var("IRONMEM_NO_DAEMON");
    }
}
