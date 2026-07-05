//! Dashboard HTTP server — loopback-only, read-only hyper server.
//!
//! Security invariants:
//! - Default bind is `127.0.0.1`. A non-loopback `--host` is rejected unless
//!   `--allow-non-loopback` is set.
//! - Each request opens a fresh read-only `Database` inside `spawn_blocking`.
//!   A `Database` / rusqlite `Connection` MUST NOT cross an `.await` or be
//!   shared across hyper tasks.
//! - No request handler calls any write path.

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use hyper_util::server::graceful::GracefulShutdown;
use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

use crate::db::schema::{Database, LATEST_SCHEMA_VERSION};
use crate::error::MemoryError;

use super::data::WarmingStatus;
use super::routes::handle_request;

/// Maximum time a connection may take to send its request headers before it is
/// dropped. Bounds slow-header (slowloris-style) connection holding.
const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(10);

/// Immutable server state shared (read-only) across all request tasks.
#[derive(Clone)]
pub(crate) struct ServerState {
    pub(crate) db_path: Arc<PathBuf>,
    pub(crate) schema_version: i64,
    /// Warming status (`ready`/`missing`/`corrupt`/`unreadable`), resolved ONCE
    /// at startup. The model checksum is ~hundreds of MB of I/O, so it must not
    /// run per request — the read path stays pure and the handler just echoes
    /// this snapshot. Reflects the cache state at launch; restart to re-check
    /// after warming.
    pub(crate) model_status: WarmingStatus,
}

/// Configuration for the dashboard server.
pub struct DashboardConfig {
    pub db_path: PathBuf,
    pub host: IpAddr,
    pub port: u16,
    pub allow_non_loopback: bool,
    pub json_startup: bool,
    /// Embed-model cache dir, resolved by the caller the same way `Config` does
    /// (env `IRONMEM_MODEL_DIR` else `model_cache_dir()`). Used only to report
    /// warming status; never written to.
    pub model_dir: PathBuf,
    /// Test/child-process lifecycle hook: shut down when stdin reaches EOF.
    ///
    /// This is disabled for normal CLI use so a background dashboard is not tied
    /// to whatever stdin its launcher happened to provide.
    pub exit_on_stdin_close: bool,
}

impl DashboardConfig {
    /// Validate the host binding against the loopback security policy.
    ///
    /// Returns an error when `host` is non-loopback and `allow_non_loopback` is
    /// false. If `allow_non_loopback` is true, prints a warning to stderr.
    pub fn validate_host(&self) -> Result<(), MemoryError> {
        if !self.host.is_loopback() && !self.allow_non_loopback {
            return Err(MemoryError::Validation(format!(
                "non-loopback host {} rejected; pass --allow-non-loopback to override \
                 (WARNING: exposes the dashboard to the network)",
                self.host
            )));
        }
        if !self.host.is_loopback() && self.allow_non_loopback {
            eprintln!(
                "WARNING: ironmem dashboard is bound to {} (non-loopback). \
                 This exposes the dashboard to any host that can reach this address. \
                 The dashboard has no authentication; use only on trusted networks.",
                self.host
            );
        }
        Ok(())
    }
}

/// Run the dashboard server until Ctrl-C.
///
/// Opens the DB read-only, validates the schema version, binds the listener,
/// and serves requests via [`handle_request`]. Clean shutdown on Ctrl-C.
pub async fn run_dashboard(cfg: DashboardConfig) -> Result<(), MemoryError> {
    cfg.validate_host()?;
    let mut parent_stdin_closed = spawn_parent_stdin_close_watcher(cfg.exit_on_stdin_close);

    // Open and schema-check the DB before binding the port so startup errors
    // are surfaced before we accept any connections. The embed-model warming
    // status is resolved here too (one checksum pass at startup) so the request
    // path never pays the model-read cost.
    let startup = async {
        let path = cfg.db_path.clone();
        let model_dir = cfg.model_dir.clone();
        tokio::task::spawn_blocking(move || -> Result<(i64, WarmingStatus), MemoryError> {
            let db = Database::open_read_only(&path)?;
            let ver = db.schema_version()?;
            if ver != LATEST_SCHEMA_VERSION {
                return Err(MemoryError::Validation(format!(
                    "schema version mismatch: expected {LATEST_SCHEMA_VERSION}, found {ver}; \
                     run `ironmem migrate` first"
                )));
            }
            let raw = ironrace_embed::embedder::model_status(&model_dir);
            // The HTTP label intentionally drops any detail; log the underlying
            // cause server-side so an operator can diagnose a non-Ready cache.
            match &raw {
                ironrace_embed::embedder::ModelStatus::Unreadable(detail) => {
                    eprintln!("ironmem dashboard: embed-model cache unreadable: {detail}");
                }
                ironrace_embed::embedder::ModelStatus::Corrupt => {
                    eprintln!(
                        "ironmem dashboard: embed-model cache checksum mismatch (corrupt); \
                         re-download with `ironmem reembed`"
                    );
                }
                _ => {}
            }
            Ok((ver, WarmingStatus::from(&raw)))
        })
        .await
        .map_err(|e| MemoryError::Validation(format!("spawn_blocking: {e}")))?
    };
    let (schema_version, model_status) = if let Some(parent_closed) = parent_stdin_closed.as_mut() {
        tokio::select! {
            result = startup => result?,
            _ = parent_closed => {
                eprintln!("ironmem dashboard: parent stdin closed; shutting down.");
                return Ok(());
            }
        }
    } else {
        startup.await?
    };

    let addr = SocketAddr::new(cfg.host, cfg.port);
    let listener = TcpListener::bind(addr).await.map_err(MemoryError::Io)?;
    let bound_addr = listener.local_addr().map_err(MemoryError::Io)?;
    let url = format!("http://{bound_addr}");

    if cfg.json_startup {
        let meta = serde_json::json!({
            "url": url,
            "db_path": cfg.db_path.display().to_string(),
            "schema_version": schema_version,
        });
        println!("{}", serde_json::to_string(&meta)?);
    } else {
        eprintln!(
            "ironmem dashboard: {url}  (db: {db}  schema: v{schema_version})",
            db = cfg.db_path.display()
        );
        eprintln!("Press Ctrl-C to stop.");
    }

    let state = Arc::new(ServerState {
        db_path: Arc::new(cfg.db_path),
        schema_version,
        model_status,
    });

    // Ctrl-C signal for clean shutdown.
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::pin!(shutdown);
    let parent_shutdown = async {
        if let Some(parent_closed) = parent_stdin_closed.as_mut() {
            let _ = parent_closed.await;
        } else {
            std::future::pending::<()>().await;
        }
    };
    tokio::pin!(parent_shutdown);
    let graceful = GracefulShutdown::new();

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _peer)) => {
                        let state = Arc::clone(&state);
                        let io = TokioIo::new(stream);
                        let watcher = graceful.watcher();
                        tokio::spawn(async move {
                            let svc = service_fn(move |req| {
                                let state = Arc::clone(&state);
                                async move { handle_request(req, state).await }
                            });
                            let conn = http1::Builder::new()
                                // A timer is required for `header_read_timeout`
                                // to fire; without it hyper panics at runtime.
                                .timer(TokioTimer::new())
                                .header_read_timeout(HEADER_READ_TIMEOUT)
                                .serve_connection(io, svc);
                            if let Err(e) = watcher.watch(conn).await {
                                tracing::debug!("dashboard connection error: {e}");
                            }
                        });
                    }
                    Err(e) => {
                        tracing::warn!("dashboard accept error: {e}");
                    }
                }
            }
            _ = &mut shutdown => {
                eprintln!("\nironmem dashboard: shutting down.");
                break;
            }
            _ = &mut parent_shutdown => {
                eprintln!("\nironmem dashboard: parent stdin closed; shutting down.");
                break;
            }
        }
    }

    graceful.shutdown().await;
    Ok(())
}

fn spawn_parent_stdin_close_watcher(enabled: bool) -> Option<tokio::task::JoinHandle<()>> {
    if !enabled {
        return None;
    }

    Some(tokio::spawn(async {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 1];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn cfg_for(host: IpAddr, allow_non_loopback: bool) -> DashboardConfig {
        DashboardConfig {
            db_path: PathBuf::from("/tmp/irrelevant.db"),
            host,
            port: 0,
            allow_non_loopback,
            json_startup: false,
            model_dir: PathBuf::from("/tmp/irrelevant-models"),
            exit_on_stdin_close: false,
        }
    }

    #[test]
    fn loopback_host_is_accepted() {
        let cfg = DashboardConfig {
            db_path: PathBuf::from("/tmp/irrelevant.db"),
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            allow_non_loopback: false,
            json_startup: false,
            model_dir: PathBuf::from("/tmp/irrelevant-models"),
            exit_on_stdin_close: false,
        };
        assert!(cfg.validate_host().is_ok());
    }

    #[test]
    fn non_loopback_without_flag_is_rejected() {
        let cfg = DashboardConfig {
            db_path: PathBuf::from("/tmp/irrelevant.db"),
            host: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            port: 0,
            allow_non_loopback: false,
            json_startup: false,
            model_dir: PathBuf::from("/tmp/irrelevant-models"),
            exit_on_stdin_close: false,
        };
        let err = cfg.validate_host().unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("non-loopback") && msg.contains("allow-non-loopback"),
            "error must mention the flag: {msg}"
        );
    }

    #[test]
    fn non_loopback_with_flag_prints_warning_but_succeeds() {
        let cfg = DashboardConfig {
            db_path: PathBuf::from("/tmp/irrelevant.db"),
            host: IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)),
            port: 0,
            allow_non_loopback: true,
            json_startup: false,
            model_dir: PathBuf::from("/tmp/irrelevant-models"),
            exit_on_stdin_close: false,
        };
        // Should not error; warning goes to stderr (not captured here).
        assert!(cfg.validate_host().is_ok());
    }

    #[test]
    fn validate_host_rejects_non_loopback_without_flag_and_allows_with_flag() {
        let host = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
        assert!(cfg_for(host, false).validate_host().is_err());
        assert!(cfg_for(host, true).validate_host().is_ok());
    }

    #[test]
    fn ipv6_loopback_is_accepted_and_non_loopback_v6_is_rejected() {
        use std::net::Ipv6Addr;
        // ::1 is the IPv6 loopback — must be accepted without the flag.
        let loopback = IpAddr::V6(Ipv6Addr::LOCALHOST);
        assert!(cfg_for(loopback, false).validate_host().is_ok());

        // 2001:db8::1 is a documentation (non-loopback) address — must be
        // rejected unless --allow-non-loopback is set.
        let non_loopback = IpAddr::V6(Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 1));
        let err = cfg_for(non_loopback, false).validate_host().unwrap_err();
        let msg = format!("{err}");
        assert!(
            msg.contains("non-loopback") && msg.contains("allow-non-loopback"),
            "v6 rejection must mention the flag: {msg}"
        );
        assert!(cfg_for(non_loopback, true).validate_host().is_ok());
    }

    #[test]
    fn open_read_only_fails_on_missing_db_for_startup() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        let result = Database::open_read_only(&path);
        assert!(result.is_err());
    }
}
