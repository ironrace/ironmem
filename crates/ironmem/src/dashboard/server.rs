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
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;

use crate::db::schema::{Database, LATEST_SCHEMA_VERSION};
use crate::error::MemoryError;

use super::routes::handle_request;

/// Immutable server state shared (read-only) across all request tasks.
#[derive(Clone)]
pub struct ServerState {
    pub db_path: Arc<PathBuf>,
    pub schema_version: i64,
}

/// Configuration for the dashboard server.
pub struct DashboardConfig {
    pub db_path: PathBuf,
    pub host: IpAddr,
    pub port: u16,
    pub allow_non_loopback: bool,
    pub json_startup: bool,
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

    // Open and schema-check the DB before binding the port so startup errors
    // are surfaced before we accept any connections.
    let schema_version = {
        let path = cfg.db_path.clone();
        tokio::task::spawn_blocking(move || -> Result<i64, MemoryError> {
            let db = Database::open_read_only(&path)?;
            let ver = db.schema_version()?;
            if ver != LATEST_SCHEMA_VERSION {
                return Err(MemoryError::Validation(format!(
                    "schema version mismatch: expected {LATEST_SCHEMA_VERSION}, found {ver}; \
                     run `ironmem migrate` first"
                )));
            }
            Ok(ver)
        })
        .await
        .map_err(|e| MemoryError::Validation(format!("spawn_blocking: {e}")))??
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
    });

    // Ctrl-C signal for clean shutdown.
    let shutdown = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            result = listener.accept() => {
                match result {
                    Ok((stream, _peer)) => {
                        let state = Arc::clone(&state);
                        let io = TokioIo::new(stream);
                        tokio::spawn(async move {
                            let svc = service_fn(move |req| {
                                let state = Arc::clone(&state);
                                async move { handle_request(req, state).await }
                            });
                            if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
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
        }
    }

    Ok(())
}

/// Thin helper used by the integration smoke test to validate host rejection
/// without starting the full server. Exported so `main.rs` and tests can reuse.
pub fn validate_host_only(host: IpAddr, allow_non_loopback: bool) -> Result<(), MemoryError> {
    if !host.is_loopback() && !allow_non_loopback {
        return Err(MemoryError::Validation(format!(
            "non-loopback host {} rejected; pass --allow-non-loopback to override",
            host
        )));
    }
    Ok(())
}

/// Open the database read-only and return its schema version.
/// Exported for use from `main.rs` startup checks.
pub fn open_and_check_schema(path: &Path) -> Result<(Database, i64), MemoryError> {
    let db = Database::open_read_only(path)?;
    let ver = db.schema_version()?;
    Ok((db, ver))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn loopback_host_is_accepted() {
        let cfg = DashboardConfig {
            db_path: PathBuf::from("/tmp/irrelevant.db"),
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: 0,
            allow_non_loopback: false,
            json_startup: false,
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
        };
        // Should not error; warning goes to stderr (not captured here).
        assert!(cfg.validate_host().is_ok());
    }

    #[test]
    fn validate_host_only_rejects_non_loopback() {
        let result = validate_host_only(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), false);
        assert!(result.is_err());
        let result_ok = validate_host_only(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)), true);
        assert!(result_ok.is_ok());
    }

    #[test]
    fn open_and_check_schema_fails_on_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("missing.db");
        let result = open_and_check_schema(&path);
        assert!(result.is_err());
    }
}
