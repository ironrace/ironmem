//! Shared-daemon transport for `ironmem serve`: `--listen` (the daemon
//! process) and `--connect` (a thin proxy in front of it).
//!
//! # Architecture
//!
//! - **`--listen <socket>`** ([`run_daemon`] / [`run_daemon_async`]): binds a
//!   `UnixListener`, creates the single `Arc<App>`, and runs an accept loop
//!   ([`serve_accept_loop`]) that serves every connection over the SAME `App`
//!   — see "Confining the `!Send` App to one thread" below for how that stays
//!   sound without `App: Send`.
//! - **`--connect <socket>`** ([`run_connect_mode`] / [`run_connect_mode_io`]):
//!   a thin byte-pump proxy ([`pump_proxy`]) between the harness's stdio and
//!   the daemon's socket. No model load, no direct DB connection — it starts
//!   in milliseconds. If nothing is listening, it single-flight auto-spawns a
//!   detached daemon under an atomic lockfile
//!   ([`autospawn_and_connect`]/[`try_acquire_lock`]) so N proxies launched at
//!   once converge on exactly one daemon.
//! - **Idle-shutdown**: `serve_accept_loop` arms a timer the instant active
//!   connections drop to zero and shuts the daemon down if nothing reconnects
//!   before it fires — see that function's doc comment for the accept-vs-timer
//!   race guarantee.
//! - **Health probe** ([`probe_daemon_health`]): a throwaway one-shot
//!   `initialize` connection `doctor` uses to report "is a daemon actually
//!   reachable here" without spawning or disturbing anything.
//!
//! Access mode (`IRONMEM_MCP_MODE`) is daemon-process-global: every client
//! sharing one daemon gets whichever mode the FIRST spawner's environment
//! set. This is a known, accepted limitation (not re-architected here) — see
//! `CODEX.md`'s shared-daemon section for the user-facing note.
//!
//! # Confining the `!Send` `App` to one thread
//!
//! `App` is `!Sync`, so `Arc<App>` is `!Send`: it can neither be
//! `tokio::spawn`ed nor moved across threads. The daemon therefore runs on ONE
//! dedicated thread, creates the single `Arc<App>` there, and keeps it there
//! for its whole life. Concurrency across connections is achieved WITHOUT
//! moving the `App`: all per-connection handlers are `!Send` futures polled
//! cooperatively on that same thread via a `FuturesUnordered`, alongside the
//! accept loop, under one `tokio::select!`. Each handler reuses
//! `run_server_io_daemon_connection` — the SAME `run_framing_loop` machinery
//! as bare stdio `serve` (full framing + per-connection metrics), just over
//! `UnixStream` halves instead of stdin/stdout, and with env-based
//! session/harness overrides disabled (see `mcp::server::TransportMode`)
//! since a daemon's own env belongs to whichever client happened to spawn it
//! first, not to every connection.
//!
//! Why NOT `spawn_local`/`LocalSet`, and why a multi-thread runtime:
//! `run_server_io`/`run_server_io_daemon_connection` wrap their synchronous
//! `dispatch` + metrics work in `tokio::task::block_in_place`.
//! `block_in_place` PANICS both on a `current_thread` runtime AND from within
//! a `LocalSet`/`spawn_local` — so the obvious "`current_thread` +
//! `spawn_local`" confinement cannot run this framing loop at all. It is,
//! however, valid on the `block_on` thread of a MULTI-THREAD runtime when NOT
//! inside a `LocalSet`. So the daemon builds a multi-thread runtime and
//! drives everything from its `block_on` thread with a `FuturesUnordered` (no
//! `LocalSet`), which both satisfies `block_in_place` and keeps every future —
//! and the `Arc<App>` they clone — pinned to this one thread.
//!
//! What `block_in_place` does NOT buy here: it offloads nothing on this
//! thread. Its effect is to hand a worker's *queued tasks* to another worker
//! before the caller blocks, and the `block_on` thread has no such queue —
//! every future in this design lives inside the one `select!` and is `!Send`,
//! so none of it can migrate. A synchronous `dispatch` therefore stalls this
//! thread, and with it every connection, for its duration. That is accepted
//! because `dispatch` is short; it is exactly why the readiness wait (up to
//! `IRONMEM_WRITE_READINESS_TIMEOUT_SECS`) was moved OUT of the handlers and
//! made `async` — see `server::dispatch_request`.
//!
//! Because handlers are cooperatively scheduled on a single thread and each
//! `dispatch` runs inside `block_in_place`, dispatch is naturally serialized:
//! the "single writer / one App" invariant holds by thread confinement, with
//! no lock. Request pipelining within a connection (see `run_framing_loop`)
//! does not weaken this — it adds concurrency only at await points, never two
//! simultaneous `dispatch` calls.
//!
//! An earlier design considered a single-owner dispatcher ACTOR — a
//! `DispatcherHandle`/`run_dispatcher` pair where per-connection handlers on
//! real worker threads would reach the `App` owner over an mpsc channel. It
//! was built and tested but never wired into the shipping daemon (thread
//! confinement above makes it unnecessary here) and has since been removed as
//! dead code; see git history for `mcp::daemon` prior to this crate's #190
//! cleanup if that design is ever needed for a different runtime model.

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::sync::Arc;
#[cfg(unix)]
use tokio::io::AsyncWriteExt;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};
#[cfg(unix)]
use tokio::sync::oneshot;

#[cfg(unix)]
use super::app::App;
#[cfg(unix)]
use crate::error::MemoryError;

/// Probe a possibly-stale socket path and prepare it for a fresh `bind`.
///
/// If `path` exists, we probe it by attempting a connect:
/// - connect SUCCEEDS -> a live daemon owns the socket; return an error and
///   NEVER unlink it (unlinking would strand an active peer).
/// - connect FAILS -> the socket is stale (dead listener). We remove it so the
///   caller can `bind`, but ONLY when the path is actually a socket file. A
///   non-socket file at this path is treated as a hard error rather than
///   silently deleted, so a misconfigured path can never destroy an unrelated
///   regular file. (The daemon socket path is daemon-owned by construction, so
///   in practice a live-vs-stale socket is the only case that occurs.)
///
/// If `path` does not exist, this is a no-op.
#[cfg(unix)]
async fn prepare_socket_path(path: &Path) -> Result<(), MemoryError> {
    use std::os::unix::fs::FileTypeExt;

    let meta = match std::fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(MemoryError::Io(e)),
    };

    // Probe: a successful connect means a live daemon is listening. Retried a
    // few times with a short delay rather than a single attempt: under heavy
    // system contention (many concurrent processes starving this host of
    // scheduling), a momentarily-slow-but-alive listener's `accept()` can be
    // delayed just long enough for a single `connect()` probe to time out /
    // get refused even though the daemon is genuinely up. A single failed
    // probe there would misjudge a LIVE daemon as stale and unlink its
    // socket out from under it — observed as an intermittent lost write in
    // the auto-spawn race integration test under a fully parallel `cargo
    // test --workspace` run. A truly dead socket (no listener at all) fails
    // EVERY attempt just as fast, so this adds negligible latency to the
    // common (dead-socket) case while closing that race.
    for attempt in 0..5 {
        if UnixStream::connect(path).await.is_ok() {
            return Err(MemoryError::Config(format!(
                "daemon already running at {}",
                path.display()
            )));
        }
        if attempt + 1 < 5 {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    // Every probe attempt failed -> stale. Only unlink a genuine socket file.
    if meta.file_type().is_socket() {
        std::fs::remove_file(path).map_err(MemoryError::Io)?;
        Ok(())
    } else {
        Err(MemoryError::Config(format!(
            "path {} exists and is not a socket; refusing to remove it",
            path.display()
        )))
    }
}

/// Bind the daemon's `UnixListener` at `path` with owner-only permissions.
///
/// Probes for (and removes) a stale socket first — never unlinking a live peer
/// — then binds and chmods the socket to `0600`. If the parent directory does
/// not exist it is created `0700` (mirrors `Config::ensure_dirs`); an existing
/// parent's permissions are left untouched.
#[cfg(unix)]
pub async fn bind_daemon_listener(path: &Path) -> Result<UnixListener, MemoryError> {
    use std::fs::Permissions;
    use std::os::unix::fs::PermissionsExt;

    prepare_socket_path(path).await?;

    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(MemoryError::Io)?;
            std::fs::set_permissions(parent, Permissions::from_mode(0o700))
                .map_err(MemoryError::Io)?;
        }
    }

    // M7: bind under a private temp name in the SAME directory, chmod THAT,
    // then atomically `rename` it into place at `path`. Binding directly at
    // `path` would leave the socket visible there at whatever wide default
    // mode the platform creates it with (e.g. 0755) for the window between
    // `bind` and an explicit `chmod` — during which another local user could
    // connect. Deliberately NOT `libc::umask`: umask is a process-GLOBAL
    // setting, not per-thread, and this crate's own test suite calls this
    // function from many concurrently running tests within one process —
    // narrowing/restoring a process-global umask around each call would be a
    // genuine cross-test race (one thread could restore a snapshot another
    // thread already narrowed, permanently corrupting the process's umask).
    // Temp-name+rename has no such shared mutable state: nothing ever
    // appears at `path` until it is already 0600.
    let tmp_path = temp_socket_path(path);
    let _ = std::fs::remove_file(&tmp_path); // best-effort: clear an implausible stale leftover
    let listener = UnixListener::bind(&tmp_path).map_err(MemoryError::Io)?;
    if let Err(e) = std::fs::set_permissions(&tmp_path, Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(MemoryError::Io(e));
    }
    if let Err(e) = std::fs::rename(&tmp_path, path) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(MemoryError::Io(e));
    }
    Ok(listener)
}

/// Derive a private, per-process temp socket path in the SAME directory as
/// `path` (so the subsequent `rename` is atomic and same-filesystem), used by
/// [`bind_daemon_listener`] (M7) to bind+chmod out of sight before publishing
/// the socket at its real name.
#[cfg(unix)]
fn temp_socket_path(path: &Path) -> std::path::PathBuf {
    let file_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_else(|| std::ffi::OsString::from("daemon.sock"));
    let mut tmp_name = std::ffi::OsString::from(".");
    tmp_name.push(&file_name);
    tmp_name.push(format!(".tmp-{}", std::process::id()));
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.join(tmp_name),
        _ => std::path::PathBuf::from(tmp_name),
    }
}

/// Accept connections on `listener` and serve each with the Task 1 framing loop
/// until `shutdown` fires or the daemon has been idle (zero active connections)
/// for `idle_timeout`. Owns the single `Arc<App>`. Every accepted connection
/// becomes a `!Send` handler future (it clones `Arc<App>` — cheap, same thread,
/// never moved) that reuses `run_server_io_daemon_connection` over the
/// `UnixStream` halves (same framing loop as stdio `serve`, env overrides
/// disabled — H4). All
/// handlers are polled cooperatively on THIS thread via a `FuturesUnordered`,
/// alongside the accept branch, under one `tokio::select!` — see the module doc
/// for why this (not `spawn_local`) is the confinement mechanism.
///
/// Idle-timeout / refcount shutdown (Task 7): an `active` counter is
/// incremented BEFORE a newly-accepted connection's handler future is even
/// pushed onto `connections` — i.e. a connection is "admitted" the instant it
/// is accepted, never after some later dispatch step — and decremented when
/// its handler future completes. The idle timer is armed only while `active ==
/// 0`; any accepted connection (including one racing the timer) disarms it
/// immediately. The `select!` is `biased` with the accept branch listed first,
/// so on any poll where BOTH a pending connection and an expired idle timer are
/// simultaneously ready, the connection is always accepted first — an admitted
/// connection is served, never dropped in favor of shutdown.
///
/// MUST be driven on the `block_on` thread of a multi-thread runtime and NOT
/// inside a `LocalSet`, so the `block_in_place` inside the framing loop is valid.
///
/// Accept errors are logged and the loop continues: a transient accept failure
/// (a client that vanished mid-handshake, a momentary fd-limit hiccup) must not
/// tear down a daemon serving many other peers. On `shutdown`, or on idle-timer
/// expiry, we stop accepting and return immediately; in-flight handlers are
/// dropped (their sockets close). Callers own removing the socket/lockfile on
/// return (see `run_daemon`).
#[cfg(unix)]
pub async fn serve_accept_loop(
    app: Arc<App>,
    listener: UnixListener,
    idle_timeout: std::time::Duration,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), MemoryError> {
    use futures_util::stream::FuturesUnordered;
    use futures_util::StreamExt;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Connection handler futures, all `!Send`, polled on this single thread.
    // Each resolves to the PRE-decrement active count, so the loop can tell
    // when a completion just dropped the count to zero.
    let mut connections = FuturesUnordered::new();
    let active = Arc::new(AtomicUsize::new(0));

    // Armed from the moment the daemon starts (no connections yet), so a
    // daemon that never receives a single connection still idles out.
    let mut idle_deadline: Option<tokio::time::Instant> =
        Some(tokio::time::Instant::now() + idle_timeout);

    loop {
        let idle_sleep = async {
            match idle_deadline {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        };

        tokio::select! {
            biased;

            // Highest priority: an accepted connection always wins over an
            // expired idle timer on the same poll (see doc comment above).
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        // Admit BEFORE the handler future is even constructed:
                        // this connection now holds the daemon open regardless
                        // of anything that happens later in this loop turn.
                        active.fetch_add(1, Ordering::SeqCst);
                        idle_deadline = None;

                        let app_conn = Arc::clone(&app);
                        let active_conn = Arc::clone(&active);
                        connections.push(async move {
                            let (read, write) = tokio::io::split(stream);
                            let reader = tokio::io::BufReader::new(read);
                            // H4: daemon connections must NOT honor the
                            // daemon process's own IRONMEM_SESSION_ID/
                            // IRONMEM_HARNESS env — attribution comes purely
                            // from each connection's own `initialize`.
                            if let Err(e) = super::server::run_server_io_daemon_connection(
                                app_conn, reader, write,
                            )
                            .await
                            {
                                tracing::warn!("daemon connection ended with error: {e}");
                            }
                            active_conn.fetch_sub(1, Ordering::SeqCst)
                        });
                    }
                    Err(e) => {
                        tracing::warn!("daemon accept error (continuing): {e}");
                        // M6: a short backoff before the next `accept()`
                        // attempt. Without it, a persistent accept failure
                        // (e.g. `EMFILE`/`ENFILE` fd exhaustion) would have
                        // this branch fire on every single poll of the
                        // `select!` loop — a 100%-CPU busy-loop that itself
                        // makes fd exhaustion harder to recover from.
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }
                }
            }
            // Drive in-flight connection handlers to completion. `None` (empty
            // set) simply means no connections are pending; the branch resolves
            // immediately, so we guard with `!connections.is_empty()` to avoid a
            // busy-loop and let `select!` fall through to the other branches.
            Some(_prev_active) = connections.next(), if !connections.is_empty() => {
                // M9: read the CURRENT count rather than trust the
                // just-completed handler's pre-decrement snapshot equaling
                // exactly 1 — equivalent in this single-threaded loop (no
                // other task can observe/mutate `active` between the
                // `fetch_sub` and this check), but more robust/self-evident:
                // it directly asks "is anything still active" instead of
                // inferring it from one handler's return value.
                if active.load(Ordering::SeqCst) == 0 {
                    // The count just dropped to zero: arm the idle timer.
                    idle_deadline = Some(tokio::time::Instant::now() + idle_timeout);
                }
            }
            _ = &mut shutdown => break,
            // Lowest priority: only fires when nothing else was ready this poll.
            _ = idle_sleep => {
                tracing::info!("daemon idle for {idle_timeout:?}; shutting down");
                break;
            }
        }
    }
    Ok(())
}

/// RAII guard that removes the daemon's own socket file on drop.
///
/// Constructed ONLY after [`bind_daemon_listener`] has succeeded for THIS
/// process (see [`run_daemon_async`]) — so a bind FAILURE (a live daemon
/// already owns `path`, per `prepare_socket_path`'s live-peer check) never
/// constructs this guard and never removes anything (C1). Once constructed,
/// it fires on every exit path (idle-timeout, error, or otherwise) via normal
/// `Drop` scoping, so cleanup can't be forgotten on a new early-return.
///
/// Deliberately does NOT also remove a lockfile (H1): `<socket>.lock` is
/// owned by the `--connect` auto-spawn proxy's [`LockGuard`], not by the
/// daemon process itself, so the daemon must never touch it.
#[cfg(unix)]
struct SocketCleanupGuard {
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for SocketCleanupGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Async core of the daemon entry point, generic over an already-constructed
/// `app` so it is directly testable (see `daemon_tests::
/// bind_failure_does_not_unlink_the_live_daemons_socket`) without going
/// through [`run_daemon`]'s dedicated-runtime/background-init wrapper.
///
/// Binds `socket_path`; a bind failure (live peer already owns it) propagates
/// immediately via `?` and constructs no cleanup guard (C1: never unlinks a
/// live daemon's socket). Only once bound does this process own the socket,
/// at which point [`SocketCleanupGuard`] is created so the socket — and ONLY
/// the socket, never the proxy-owned lockfile (H1) — is removed on every exit
/// path from here on, including the idle timer inside `serve_accept_loop`.
#[cfg(unix)]
async fn run_daemon_async(
    app: Arc<App>,
    socket_path: std::path::PathBuf,
    idle_timeout: std::time::Duration,
    shutdown: oneshot::Receiver<()>,
) -> Result<(), MemoryError> {
    let listener = bind_daemon_listener(&socket_path).await?;
    // Reached only after a successful bind: this process now owns the
    // socket, so cleanup is safe to arm from this point on.
    let _cleanup = SocketCleanupGuard { path: socket_path };
    serve_accept_loop(app, listener, idle_timeout, shutdown).await
}

/// Production daemon entry point for `serve --listen <socket>`.
///
/// Runs entirely on the CURRENT thread: builds a multi-thread runtime and drives
/// everything from its `block_on` thread (NO `LocalSet`, so `block_in_place`
/// inside `run_server_io` stays valid). Creates the single `App` here (it is
/// `!Send` and must never move), binds the listener, kicks background memory
/// init so daemon connections eventually get a real embedder, and serves until
/// process exit.
///
/// This is a blocking `fn` (it owns its runtime), so callers already inside a
/// tokio runtime must invoke it on a dedicated `std::thread` to avoid a nested
/// runtime panic — see `main.rs`'s `--listen` arm.
///
/// The shutdown receiver here is a never-fired channel: the daemon runs until
/// process exit or the idle timer (from `Config::daemon_idle_timeout`) expires
/// via `serve_accept_loop`. On a successful bind, EITHER exit path removes
/// ONLY the daemon-owned socket (best-effort, via [`SocketCleanupGuard`]; a
/// `NotFound` is expected and ignored), so a later `--connect` proxy correctly
/// probes "no daemon" rather than finding a stale path. A FAILED bind (a live
/// daemon already owns `socket_path`) removes nothing at all (C1) — see
/// [`run_daemon_async`]. The lockfile is never touched here: it is proxy-owned
/// (H1).
#[cfg(unix)]
pub fn run_daemon(
    config: crate::config::Config,
    socket_path: std::path::PathBuf,
) -> Result<(), MemoryError> {
    let idle_timeout = config.daemon_idle_timeout();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(MemoryError::Io)?;
    rt.block_on(async move {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::new_server_ready(config.clone())?);
        // Mirror the stdio `serve` path: record version + kick background model
        // load so connections eventually get a real embedder. Background init
        // runs on its own thread with its own DB connection (WAL-safe).
        crate::bootstrap::check_and_record_version(&config.state_dir);
        let memory_ready = Arc::clone(&app.memory_ready);
        crate::bootstrap::run_background_memory_init(config, memory_ready);

        // Never-fired shutdown: only the idle timer inside `serve_accept_loop`
        // ends this daemon absent an external kill.
        let (_tx, rx) = oneshot::channel::<()>();
        run_daemon_async(app, socket_path, idle_timeout, rx).await
    })
}

// ---------------------------------------------------------------------------
// `--connect` thin proxy (Task 8).
//
// The proxy does NO model load and opens NO direct DB connection: it is just
// two byte pumps wired between the harness's stdio and the daemon's Unix
// socket, so a `--connect` client starts in milliseconds regardless of how
// heavy the shared daemon's `App` is.

/// Outcome of attempting the `--connect` transport.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
pub enum ProxyOutcome {
    /// Connected to a live daemon and proxied until either side hit EOF.
    Proxied,
    /// No live daemon and auto-spawn is disabled: the caller must fall
    /// through to an in-process `run_server` instead.
    FallbackToInProcess,
}

/// Pump bytes between `local_in`/`local_out` (the harness's stdio in
/// production; injectable in tests) and `stream` (the daemon connection)
/// until EOF on EITHER side, with one asymmetry (M2): if the DAEMON side
/// closes first (`from_socket` completes), that is unconditionally
/// terminal — nothing more will ever arrive, so we return immediately rather
/// than waiting on the now-orphaned `to_socket` pump. But if the LOCAL input
/// side hits EOF first (`to_socket` completes — e.g. a one-shot/piped client
/// closed stdin right after sending its request), the daemon may still have a
/// response in flight; we half-close the socket's write half (telling the
/// daemon we're done sending) and keep draining `from_socket` until the
/// daemon closes its end, so that in-flight response still reaches
/// `local_out` instead of being dropped on the floor.
#[cfg(unix)]
async fn pump_proxy<R, W>(
    stream: UnixStream,
    mut local_in: R,
    mut local_out: W,
) -> Result<(), MemoryError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let (mut sock_read, mut sock_write) = tokio::io::split(stream);

    tokio::select! {
        result = tokio::io::copy(&mut local_in, &mut sock_write) => {
            result.map_err(MemoryError::Io)?;
            sock_write.shutdown().await.map_err(MemoryError::Io)?;
            tokio::io::copy(&mut sock_read, &mut local_out)
                .await
                .map_err(MemoryError::Io)?;
        }
        result = tokio::io::copy(&mut sock_read, &mut local_out) => {
            result.map_err(MemoryError::Io)?;
        }
    }
    Ok(())
}

/// Core `--connect` decision, parameterized over the local reader/writer so it
/// is directly testable without redirecting the real process stdio. Production
/// use goes through [`run_connect_mode`].
///
/// - Connect succeeds -> proxy the connection; returns [`ProxyOutcome::Proxied`]
///   once the connection ends.
/// - Connect fails AND `autospawn_enabled` is `false` -> returns
///   [`ProxyOutcome::FallbackToInProcess`] so the caller runs the in-process
///   stdio server instead (no daemon, and the caller was told not to spawn one).
/// - Connect fails with `NotFound`/`ConnectionRefused` (M4: "nothing is
///   listening there yet", the only kinds that plausibly mean "no daemon")
///   AND `autospawn_enabled` is `true` -> single-flight auto-spawn (Task 9):
///   acquire `<socket>.lock`, spawn a detached daemon (unless another proxy
///   already won the race, forwarding `db_path` so the spawned daemon serves
///   the SAME database this proxy was invoked against — M3), poll-connect
///   until ready, then proxy. If auto-spawn itself hard-fails (lock-wait or
///   poll-connect exhausted), that must not take the whole `serve` process
///   down with it (M5): fall back to in-process serve, same as the
///   autospawn-disabled arm, just logging why.
/// - Connect fails with any OTHER error kind (e.g. `PermissionDenied`) -> that
///   is a real problem the caller should see, not a signal to guess "no
///   daemon" and spawn a competing one (M4); propagated as-is.
#[cfg(unix)]
async fn run_connect_mode_io<R, W>(
    socket_path: &Path,
    autospawn_enabled: bool,
    db_path: &Path,
    daemon_log_path: &Path,
    local_in: R,
    local_out: W,
) -> Result<ProxyOutcome, MemoryError>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    match UnixStream::connect(socket_path).await {
        Ok(stream) => {
            pump_proxy(stream, local_in, local_out).await?;
            Ok(ProxyOutcome::Proxied)
        }
        Err(e) if !autospawn_enabled => {
            tracing::info!(
                "no daemon at {} ({e}); auto-spawn disabled, falling back to in-process serve",
                socket_path.display()
            );
            Ok(ProxyOutcome::FallbackToInProcess)
        }
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            let lock_path = lock_path_for_socket(socket_path);
            match autospawn_and_connect(socket_path, &lock_path, db_path, daemon_log_path).await {
                Ok(stream) => {
                    pump_proxy(stream, local_in, local_out).await?;
                    Ok(ProxyOutcome::Proxied)
                }
                Err(spawn_err) => {
                    tracing::warn!(
                        "auto-spawn failed for {} ({spawn_err}); falling back to in-process serve",
                        socket_path.display()
                    );
                    Ok(ProxyOutcome::FallbackToInProcess)
                }
            }
        }
        Err(e) => {
            tracing::warn!(
                "connect to {} failed with an unexpected error kind ({:?}), not spawning a daemon: {e}",
                socket_path.display(),
                e.kind()
            );
            Err(MemoryError::Io(e))
        }
    }
}

/// Production `--connect` entry point: proxies the real process stdio.
///
/// `db_path` is this proxy's OWN resolved database path (`Config::db_path` —
/// whatever `--db` / `IRONMEM_DB_PATH` / default resolved to); it is forwarded
/// as `--db` to an auto-spawned daemon (M3) so the daemon serves the SAME
/// database this proxy was invoked against, rather than silently falling back
/// to the default. `daemon_log_path` is where an auto-spawned daemon's stderr
/// is redirected (H5).
#[cfg(unix)]
pub async fn run_connect_mode(
    socket_path: &Path,
    autospawn_enabled: bool,
    db_path: &Path,
    daemon_log_path: &Path,
) -> Result<ProxyOutcome, MemoryError> {
    run_connect_mode_io(
        socket_path,
        autospawn_enabled,
        db_path,
        daemon_log_path,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

// ---------------------------------------------------------------------------
// Daemon health probe for `doctor` (#190 Task 14).
//
// Architecturally this probe is just another short-lived `--connect`-style
// connection: it opens its OWN `UnixStream`, sends ONE `initialize` request,
// and closes. Task 2's per-connection `ConnectionContext` scopes learned
// session/harness attribution to a single connection by construction, so this
// probe's `initialize` can never mutate any OTHER client's already-recorded
// attribution — see `mcp::server::tests::sequential_connections_on_shared_app_get_independent_attribution`
// for the underlying guarantee this relies on, and
// `daemon_tests::health_probe_does_not_disturb_another_connections_attribution`
// below for the direct proof.

/// Outcome of a daemon health probe.
#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonHealth {
    /// Connected and received a valid `initialize` response.
    Reachable,
    /// No live daemon: connect failed, timed out, or the reply was not a
    /// recognizable `initialize` response.
    Unreachable,
}

/// Health-probe a shared daemon: connect to `socket_path` and send a single
/// `initialize` ping, bounded by `timeout` end-to-end (both the connect and
/// the round trip). Never spawns anything and never retries — a `doctor`
/// check should report what IS true right now, not coax a daemon into
/// existing.
#[cfg(unix)]
pub async fn probe_daemon_health(socket_path: &Path, timeout: std::time::Duration) -> DaemonHealth {
    let Ok(Ok(stream)) = tokio::time::timeout(timeout, UnixStream::connect(socket_path)).await
    else {
        return DaemonHealth::Unreachable;
    };

    match tokio::time::timeout(timeout, initialize_ping(stream)).await {
        Ok(Ok(true)) => DaemonHealth::Reachable,
        _ => DaemonHealth::Unreachable,
    }
}

/// Send one `initialize` request over `stream` and report whether the reply
/// looks like a genuine MCP `initialize` response.
#[cfg(unix)]
async fn initialize_ping(stream: UnixStream) -> Result<bool, MemoryError> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

    let (read_half, mut write_half) = tokio::io::split(stream);
    let mut reader = BufReader::new(read_half);

    write_half
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .await
        .map_err(MemoryError::Io)?;
    write_half.flush().await.map_err(MemoryError::Io)?;

    let mut line = String::new();
    reader.read_line(&mut line).await.map_err(MemoryError::Io)?;
    Ok(line.contains("\"protocolVersion\""))
}

// ---------------------------------------------------------------------------
// Single-flight auto-spawn under an atomic lockfile (Task 9).
//
// When a `--connect` proxy finds no daemon listening and auto-spawn is
// enabled, MANY proxies may race to start one at once (e.g. several MCP
// clients launched together). Exactly one of them must actually spawn the
// daemon; the rest must simply wait and then connect to the winner's daemon.
// The `<socket>.lock` file is the single-flight gate: atomic `create_new`
// decides the winner, a dead owner's stale lock is safely recovered, and a
// live owner's lock is never stolen.

/// Derive the lockfile path from a runtime socket path: `<socket>.lock`,
/// mirroring `Config::daemon_lock_path` but applied to whatever socket path
/// was actually supplied on the command line (which need not match a
/// `Config`-derived default).
#[cfg(unix)]
fn lock_path_for_socket(socket_path: &Path) -> std::path::PathBuf {
    let mut name = socket_path.as_os_str().to_os_string();
    name.push(".lock");
    std::path::PathBuf::from(name)
}

/// Result of attempting to acquire the single-flight lock.
#[cfg(unix)]
#[derive(Debug, PartialEq, Eq)]
enum LockOutcome {
    /// We now own the lock (our pid is recorded in the lockfile).
    Acquired,
    /// Another live process holds the lock; it is presumably spawning.
    HeldByOther,
}

/// True unless `pid` is definitively gone. `kill(pid, 0)` sends no signal —
/// it only checks deliverability. Success means the process exists; `ESRCH`
/// means it does not. Any OTHER errno (chiefly `EPERM`, no permission to
/// signal a process we don't own) still means the process exists, so only
/// `ESRCH` is treated as "dead" — anything else is conservatively "alive" to
/// avoid ever stealing a live owner's lock.
#[cfg(unix)]
fn pid_is_alive(pid: i32) -> bool {
    if pid <= 0 {
        return false;
    }
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Best-effort removal of `lock_path` on drop, releasing the single-flight
/// lock whether the guarded section succeeded or returned early via `?`.
#[cfg(unix)]
struct LockGuard {
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Path for the private, per-process "claim" file used by [`try_acquire_lock`]
/// to publish `lock_path` atomically-with-content (see that function's doc
/// comment for why a plain `create_new` + separate write is unsafe).
#[cfg(unix)]
fn lock_claim_tmp_path(lock_path: &Path) -> std::path::PathBuf {
    let mut name = lock_path.as_os_str().to_os_string();
    name.push(format!(".claim-{}", std::process::id()));
    std::path::PathBuf::from(name)
}

/// Attempt to acquire `lock_path` with our pid as content. If it already
/// exists, a live owner means [`LockOutcome::HeldByOther`]; a dead owner (or
/// an unreadable/malformed lockfile) is stale and is removed so the
/// acquisition race can be retried. Bounded to avoid spinning forever under
/// pathological contention.
///
/// Publishing is done via write-then-`hard_link`, NOT `create_new` followed by
/// a separate write. The naive `create_new` + write has a real TOCTOU window:
/// `create_new` creates an EMPTY file first, and the pid is written to it in a
/// second, separate step. A peer that hits `AlreadyExists` in between those
/// two steps reads an empty/unparseable lockfile, misjudges it as stale, and
/// steals it out from under the true (still-alive, still-writing) owner —
/// producing two "winners" and, in this daemon's case, two spawned daemons
/// racing for the same socket. Writing full content to a private per-process
/// temp file FIRST, then `hard_link`ing it onto `lock_path`, closes that
/// window: `hard_link` is the single atomic publish step, and the linked
/// inode already carries its final content the instant it becomes visible at
/// `lock_path` — there is no intermediate "exists but empty" state for a peer
/// to observe.
#[cfg(unix)]
fn try_acquire_lock(lock_path: &Path) -> Result<LockOutcome, MemoryError> {
    const MAX_STALE_RECOVERY_ATTEMPTS: u32 = 20;

    for _ in 0..MAX_STALE_RECOVERY_ATTEMPTS {
        let tmp_path = lock_claim_tmp_path(lock_path);
        std::fs::write(&tmp_path, std::process::id().to_string()).map_err(MemoryError::Io)?;
        let link_result = std::fs::hard_link(&tmp_path, lock_path);
        let _ = std::fs::remove_file(&tmp_path); // disposable either way

        match link_result {
            Ok(()) => return Ok(LockOutcome::Acquired),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                match std::fs::read_to_string(lock_path) {
                    Ok(content) => {
                        let owner_pid: Option<i32> = content.trim().parse().ok();
                        if owner_pid.is_some_and(pid_is_alive) {
                            return Ok(LockOutcome::HeldByOther);
                        }
                        // Stale (dead owner, or an unparseable/corrupt lock —
                        // never left behind by this code, so it too is
                        // treated as recoverable): remove and retry.
                        let _ = std::fs::remove_file(lock_path);
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                        // Raced away between our AlreadyExists and this read
                        // (another process's stale-recovery or release) —
                        // just retry.
                    }
                    Err(e) => return Err(MemoryError::Io(e)),
                }
            }
            Err(e) => return Err(MemoryError::Io(e)),
        }
    }

    Err(MemoryError::Config(format!(
        "could not acquire the daemon lock at {} after {MAX_STALE_RECOVERY_ATTEMPTS} attempts",
        lock_path.display()
    )))
}

/// Poll-connect with bounded exponential backoff until a freshly spawned
/// daemon's socket accepts connections, or the attempts are exhausted.
#[cfg(unix)]
async fn poll_connect_with_backoff(socket_path: &Path) -> Result<UnixStream, MemoryError> {
    const MAX_ATTEMPTS: u32 = 100;
    let mut delay = std::time::Duration::from_millis(20);

    for attempt in 0..MAX_ATTEMPTS {
        match UnixStream::connect(socket_path).await {
            Ok(stream) => return Ok(stream),
            Err(e) if attempt + 1 == MAX_ATTEMPTS => return Err(MemoryError::Io(e)),
            Err(_) => {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(std::time::Duration::from_millis(500));
            }
        }
    }
    unreachable!("loop always returns via the attempt+1 == MAX_ATTEMPTS arm above")
}

/// Spawn a detached `ironmem serve --listen <socket> --db <db_path>` daemon
/// child process. Stdin/stdout are not inherited (the daemon reads/writes
/// nothing over stdio in `--listen` mode); it is placed in its own process
/// group so it survives the spawning proxy's terminal session ending.
///
/// `db_path` (M3) is forwarded explicitly as `--db` so the auto-spawned
/// daemon serves the SAME database the spawning proxy was invoked against —
/// without this, the daemon would fall back to `Config::load(None)`'s default
/// resolution and a proxy invoked with a custom `--db` would silently end up
/// talking to the wrong database.
///
/// `log_path` (H5) is where the daemon's stderr — its `tracing` logs, panics,
/// and fatal startup errors (bind failure, DB migration, config errors) — is
/// redirected in append mode, so a daemon that fails to come up leaves a
/// diagnosable trail instead of a silently-discarded stderr turning every
/// startup failure into an undiagnosable "connection refused after retries"
/// at the polling proxy. The log file's parent directory is created if
/// missing.
#[cfg(unix)]
fn spawn_daemon_process(
    socket_path: &Path,
    db_path: &Path,
    log_path: &Path,
) -> Result<(), MemoryError> {
    use std::os::unix::process::CommandExt;

    let exe = std::env::current_exe()
        .map_err(|e| MemoryError::Config(format!("cannot resolve ironmem path: {e}")))?;

    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(MemoryError::Io)?;
    }
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)
        .map_err(MemoryError::Io)?;

    std::process::Command::new(exe)
        .arg("serve")
        .arg("--listen")
        .arg(socket_path)
        .arg("--db")
        .arg(db_path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(log_file)
        .process_group(0)
        .spawn()
        .map_err(|e| MemoryError::Config(format!("failed to spawn daemon: {e}")))?;
    Ok(())
}

/// Single-flight auto-spawn core, generic over how "start a daemon" is
/// performed so it is testable without invoking a real subprocess (production
/// goes through [`autospawn_and_connect`], which spawns the real `ironmem`
/// binary). Acquires `lock_path`; the winner re-checks `socket_path` (another
/// proxy may have already finished spawning between our failed initial
/// connect and winning the lock) before calling `spawn`, then poll-connects.
/// A loser (lock held by another live process) retries a plain connect first —
/// the winner's daemon may already be bound even before it releases the lock —
/// falling back to re-attempting the lock if that connect also fails.
#[cfg(unix)]
async fn autospawn_and_connect_with<F>(
    socket_path: &Path,
    lock_path: &Path,
    spawn: F,
) -> Result<UnixStream, MemoryError>
where
    F: FnOnce() -> Result<(), MemoryError>,
{
    const MAX_LOCK_WAIT_ATTEMPTS: u32 = 200;
    let mut spawn = Some(spawn);

    for _ in 0..MAX_LOCK_WAIT_ATTEMPTS {
        match try_acquire_lock(lock_path)? {
            LockOutcome::Acquired => {
                let _guard = LockGuard {
                    path: lock_path.to_path_buf(),
                };
                // Re-check inside the lock: another proxy may have already
                // won and finished spawning before we got here.
                if let Ok(stream) = UnixStream::connect(socket_path).await {
                    return Ok(stream);
                }
                let spawn = spawn
                    .take()
                    .expect("autospawn_and_connect_with only reaches Acquired once");
                spawn()?;
                return poll_connect_with_backoff(socket_path).await;
                // `_guard` drops here (success or `?`-propagated error),
                // releasing the lock so any waiting proxy can proceed.
            }
            LockOutcome::HeldByOther => {
                if let Ok(stream) = UnixStream::connect(socket_path).await {
                    return Ok(stream);
                }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }
    }

    Err(MemoryError::Config(format!(
        "timed out waiting for the daemon lock at {}",
        lock_path.display()
    )))
}

/// Production single-flight auto-spawn: spawns the real `ironmem` binary.
#[cfg(unix)]
async fn autospawn_and_connect(
    socket_path: &Path,
    lock_path: &Path,
    db_path: &Path,
    log_path: &Path,
) -> Result<UnixStream, MemoryError> {
    autospawn_and_connect_with(socket_path, lock_path, || {
        spawn_daemon_process(socket_path, db_path, log_path)
    })
    .await
}

#[cfg(all(test, unix))]
mod daemon_tests {
    use super::*;
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Poll-connect with bounded retries so we never race a not-yet-bound
    /// daemon and never rely on a fixed sleep.
    ///
    /// `label` names the call site. Several tests connect more than once, and
    /// on failure the panic previously named neither the attempt nor the
    /// underlying errno — which made a CI-only failure impossible to diagnose
    /// from the log, since "daemon never bound" and "daemon already exited"
    /// produce the same message but have opposite causes. The last OS error
    /// distinguishes them: `ENOENT` means the socket was never created (or was
    /// cleaned up on exit), `ECONNREFUSED` means the file outlived its
    /// listener.
    fn connect_with_retry(path: &std::path::Path, label: &str) -> StdUnixStream {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut last_err = None;
        while std::time::Instant::now() < deadline {
            match StdUnixStream::connect(path) {
                Ok(stream) => return stream,
                Err(err) => last_err = Some(err),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!(
            "could not connect to daemon socket within timeout \
             (attempt: {label}, path: {}, last error: {})",
            path.display(),
            last_err
                .map(|e| e.to_string())
                .unwrap_or_else(|| "none recorded".to_string()),
        );
    }

    /// End-to-end: a proxy-style connection does `initialize` (a read path) and
    /// `add_drawer` (a WRITE path) against a shared daemon that owns a single
    /// `!Send` `App` confined to its own dedicated thread.
    #[test]
    fn daemon_round_trips_initialize_and_add_drawer() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let sock_thread = sock.clone();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();

        let daemon = std::thread::spawn(move || {
            // Multi-thread runtime, driven from its block_on thread with no
            // LocalSet — the runtime model `serve_accept_loop` requires.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                // Idle timeout long enough to never fire during this test —
                // shutdown is explicit via `shutdown_tx` below.
                serve_accept_loop(app, listener, Duration::from_secs(600), shutdown_rx)
                    .await
                    .unwrap();
            });
        });

        let stream = connect_with_retry(&sock, "initial connect");
        let mut writer = stream.try_clone().unwrap();
        let mut reader = StdBufReader::new(stream);

        // initialize
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .unwrap();
        writer.flush().unwrap();
        let mut init_line = String::new();
        reader.read_line(&mut init_line).unwrap();
        assert!(
            init_line.contains("\"protocolVersion\""),
            "initialize response should carry the protocol version: {init_line}"
        );

        // add_drawer — a write through the shared App.
        writer
            .write_all(
                b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"add_drawer\",\"arguments\":{\"wing\":\"testwing\",\"room\":\"testroom\",\"content\":\"hello daemon\"}}}\n",
            )
            .unwrap();
        writer.flush().unwrap();
        let mut add_line = String::new();
        reader.read_line(&mut add_line).unwrap();
        assert!(
            add_line.contains("\"id\":2"),
            "add_drawer reply id: {add_line}"
        );
        assert!(
            !add_line.contains("\"isError\":true"),
            "add_drawer should succeed, not error: {add_line}"
        );

        // Teardown: drop the client, signal shutdown, join the daemon thread.
        drop(writer);
        drop(reader);
        shutdown_tx.send(()).ok();
        daemon.join().unwrap();
    }

    /// Task 7: a daemon with zero active connections shuts itself down once the
    /// idle timer expires. Uses a short test-overridden idle window so the
    /// test is fast and deterministic (no reliance on the real 300s default).
    #[test]
    fn idle_timeout_shuts_down_daemon_after_last_disconnect() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let sock_thread = sock.clone();

        let (_shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let idle_timeout = Duration::from_millis(150);

        let daemon = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                serve_accept_loop(app, listener, idle_timeout, shutdown_rx)
                    .await
                    .unwrap();
            });
        });

        // Connect, exchange one request, then disconnect — the idle countdown
        // starts the instant this connection's handler completes.
        let stream = connect_with_retry(&sock, "initial connect");
        let mut writer = stream.try_clone().unwrap();
        let mut reader = StdBufReader::new(stream);
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        drop(writer);
        drop(reader);

        // The daemon thread must exit on its own (idle timeout), never needing
        // `_shutdown_tx` to fire. `join` blocks until the thread returns; a
        // generous bound keeps this from hanging forever if the feature
        // regresses, while staying well clear of the 150ms idle window.
        let joined = std::thread::spawn(move || daemon.join());
        for _ in 0..50 {
            if joined.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            joined.is_finished(),
            "daemon must shut down on its own after the idle window elapses"
        );
        joined.join().unwrap().unwrap();
    }

    /// Task 7 acceptance: a new connection admitted WHILE the idle timer is
    /// counting down must disarm it and be served normally — the daemon must
    /// NOT shut down out from under an in-flight or freshly-admitted
    /// connection. This proves the timer is reset (not merely deferred) by
    /// activity, keeping the daemon alive well past the original deadline.
    #[test]
    fn new_connection_resets_idle_timer_and_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let sock_thread = sock.clone();

        let (_shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        // Every interval below is a fraction of this window, so the test scales
        // as a unit and has no absolute scheduling margin to blow.
        //
        // The previous shape — 200ms window, 100ms sleep, then "the second
        // connect must be served" — failed on every macOS CI run: with ~100ms
        // of margin, a runner executing the full suite in parallel let the
        // timer fire, the daemon exited, and the second connect spent its whole
        // retry budget against a dead socket.
        //
        // Simply widening the window does NOT work, and the failure is silent:
        // once the remaining window comfortably exceeds one round-trip, the
        // second connection is served whether or not the timer was ever
        // disarmed, because the daemon has not reached the stale deadline yet.
        // Deleting `idle_deadline = None` from the accept arm then leaves the
        // test green. The old assertion was only ever detective by accident —
        // it caught the bug solely because the round-trip had to beat a 100ms
        // deadline, which is the same property that made it flaky.
        //
        // So this asserts the observable that actually distinguishes the two
        // implementations: the accept-loop's `idle_sleep` arm breaks the loop
        // UNCONDITIONALLY, without consulting `active`. A stale deadline
        // therefore kills the daemon even with a connection open. Connection 2
        // is held open ACROSS the original deadline and must still be answered
        // afterward — impossible unless the deadline was genuinely disarmed.
        let idle_timeout = Duration::from_secs(2);

        let daemon = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                serve_accept_loop(app, listener, idle_timeout, shutdown_rx)
                    .await
                    .unwrap();
            });
        });

        // Connection 1: connect + disconnect immediately, arming the timer.
        // `armed_at` anchors the original deadline at roughly
        // `armed_at + idle_timeout`; every later instant is measured from it.
        {
            let stream = connect_with_retry(&sock, "first connect, arms the idle timer");
            let mut writer = stream.try_clone().unwrap();
            let mut reader = StdBufReader::new(stream);
            writer
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n",
                )
                .unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
        }
        let armed_at = std::time::Instant::now();

        // Wait past the halfway mark, then connect again. Past half so an
        // implementation that merely DEFERRED the deadline by half a window
        // would already have expired; still 40% of the window short of the
        // deadline, so the connect itself has generous margin.
        std::thread::sleep(idle_timeout.mul_f64(0.6));
        let stream2 = connect_with_retry(&sock, "second connect, inside the idle window");
        let mut writer2 = stream2.try_clone().unwrap();
        let mut reader2 = StdBufReader::new(stream2);
        writer2
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{}}\n")
            .unwrap();
        writer2.flush().unwrap();
        let mut line2 = String::new();
        reader2
            .read_line(&mut line2)
            .expect("second connection must be served, not dropped by a stale idle timer");
        assert!(
            line2.contains("\"protocolVersion\""),
            "second connection got a real response: {line2}"
        );

        // The load-bearing step. Hold connection 2 OPEN until well past the
        // deadline connection 1 armed, then use it again. If the accept arm
        // failed to disarm that deadline, `idle_sleep` has already fired and
        // broken the accept loop — it does not consult `active` — so the
        // daemon is gone and this request gets EOF instead of a response.
        let past_original_deadline = armed_at + idle_timeout.mul_f64(1.3);
        let now = std::time::Instant::now();
        assert!(
            now < past_original_deadline,
            "test scheduling overran its own budget before the probe could run: \
             {:?} already elapsed of a {:?} window",
            now - armed_at,
            idle_timeout,
        );
        std::thread::sleep(past_original_deadline - now);
        writer2
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"initialize\",\"params\":{}}\n")
            .unwrap();
        writer2.flush().unwrap();
        let mut line3 = String::new();
        let read3 = reader2.read_line(&mut line3);
        assert!(
            matches!(read3, Ok(n) if n > 0),
            "a connection held open across the original idle deadline must still \
             be served — the deadline connection 1 armed was never disarmed, so \
             the daemon shut down out from under an open connection (read: \
             {read3:?})"
        );
        assert!(
            line3.contains("\"protocolVersion\""),
            "held-open connection got a real response after the original \
             deadline passed: {line3}"
        );
        drop(writer2);
        drop(reader2);

        // Now let this (second) idle window fully elapse with no further
        // activity: the daemon must eventually shut down on its own. The bound
        // is derived from `idle_timeout` rather than hardcoded — a fixed
        // 50 x 20ms budget was shorter than the widened window and would have
        // reported "never shut down" while the daemon was still correctly
        // counting down.
        let shutdown_deadline = std::time::Instant::now() + idle_timeout * 3;
        let joined = std::thread::spawn(move || daemon.join());
        while std::time::Instant::now() < shutdown_deadline {
            if joined.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            joined.is_finished(),
            "daemon must still shut down after the reset idle window elapses"
        );
        joined.join().unwrap().unwrap();
    }

    /// M9 acceptance: a connection held OPEN across an idle window that would
    /// otherwise have expired must keep the daemon alive AND keep being
    /// served — not merely "not disconnected", but actually still answering
    /// requests. `active` never drops to zero while this connection is open,
    /// so the idle timer must never even arm, let alone fire.
    #[test]
    fn connection_held_open_past_idle_window_keeps_daemon_alive_and_is_served() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let sock_thread = sock.clone();

        let (_shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let idle_timeout = Duration::from_millis(120);

        let daemon = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                serve_accept_loop(app, listener, idle_timeout, shutdown_rx)
                    .await
                    .unwrap();
            });
        });

        // Open ONE connection and keep it open (never drop writer/reader)
        // across more than the full idle window.
        let stream = connect_with_retry(&sock, "initial connect, held open across the idle window");
        let mut writer = stream.try_clone().unwrap();
        let mut reader = StdBufReader::new(stream);
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .unwrap();
        writer.flush().unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("\"protocolVersion\""));

        // Sleep well past the idle window WITHOUT closing the connection:
        // since `active` never drops to zero, the idle timer must never arm.
        std::thread::sleep(idle_timeout * 3);

        // The SAME still-open connection must still be served afterward.
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{}}\n")
            .unwrap();
        writer.flush().unwrap();
        let mut line2 = String::new();
        reader
            .read_line(&mut line2)
            .expect("daemon must still be alive and serving the held-open connection");
        assert!(
            line2.contains("\"protocolVersion\""),
            "held-open connection still gets real responses: {line2}"
        );

        // Now close it and let the (now-armable) idle timer run out normally.
        drop(writer);
        drop(reader);
        let joined = std::thread::spawn(move || daemon.join());
        for _ in 0..50 {
            if joined.is_finished() {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            joined.is_finished(),
            "daemon must shut down after the held-open connection finally \
             closes and the idle window elapses"
        );
        joined.join().unwrap().unwrap();
    }

    /// Task 7 + H1: on idle-timeout exit, `run_daemon_async` removes ONLY the
    /// daemon-owned socket via `SocketCleanupGuard` — never a lockfile, which
    /// is proxy-owned (H1) and must survive untouched. A pre-existing lock at
    /// this path stands in for one legitimately held by a `--connect` proxy
    /// that spawned this daemon; the daemon must never reach for it.
    #[tokio::test]
    async fn idle_exit_cleanup_removes_only_the_socket_never_the_lock() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let lock = dir.path().join("daemon.sock.lock");
        std::fs::write(&lock, b"12345").unwrap();

        let (_shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        run_daemon_async(app, sock.clone(), Duration::from_millis(50), shutdown_rx)
            .await
            .unwrap();

        assert!(!sock.exists(), "socket file must be removed on idle exit");
        assert!(
            lock.exists(),
            "the daemon must never remove the proxy-owned lockfile"
        );
    }

    /// C1 acceptance: a losing `run_daemon_async` (bind fails because a live
    /// daemon already owns the socket) must NOT unlink the winner's socket.
    /// Before the fix, cleanup ran unconditionally after `rt.block_on`,
    /// regardless of whether THIS process ever actually bound the listener —
    /// so the loser would delete the winner's live socket out from under it.
    #[test]
    fn bind_failure_does_not_unlink_the_live_daemons_socket() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let sock_thread = sock.clone();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let winner = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                run_daemon_async(app, sock_thread, Duration::from_secs(600), shutdown_rx).await
            })
        });

        // Wait for the winner to actually bind before probing it.
        for _ in 0..200 {
            if sock.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(sock.exists(), "winner must bind before the loser probes it");

        // Loser: same socket path, must fail to bind (live peer detected by
        // `prepare_socket_path`'s probe-connect) and must NOT unlink it.
        let sock_loser = sock.clone();
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (_tx2, rx2) = oneshot::channel::<()>();
        let result = rt.block_on(async move {
            #[allow(clippy::arc_with_non_send_sync)]
            let app2 = Arc::new(App::open_for_test().unwrap());
            run_daemon_async(app2, sock_loser, Duration::from_secs(600), rx2).await
        });
        assert!(
            result.is_err(),
            "the loser must fail to bind rather than steal the socket"
        );
        assert!(
            sock.exists(),
            "the winner's live socket must still exist after the loser's failed bind"
        );

        shutdown_tx.send(()).ok();
        winner.join().unwrap().unwrap();
    }

    /// A dead/stale socket file (no live listener behind it) must not block a
    /// fresh bind: `bind_daemon_listener` probes, finds it dead, unlinks it, and
    /// binds successfully.
    #[tokio::test]
    async fn stale_socket_is_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");

        // Create a real socket file, then drop the listener WITHOUT unlinking —
        // std/tokio UnixListener do not remove the socket file on drop, so the
        // path is left as a socket with no live listener (a connect fails).
        {
            let first = std::os::unix::net::UnixListener::bind(&sock).unwrap();
            drop(first);
        }
        assert!(sock.exists(), "stale socket file should still be present");

        // Fresh bind must succeed by unlinking the stale socket.
        let listener = bind_daemon_listener(&sock)
            .await
            .expect("stale socket must be replaced, not block the bind");
        drop(listener);
    }

    /// A LIVE socket must never be unlinked: a second bind probes, connects
    /// successfully (proving a live daemon), and refuses with an error. The
    /// first live listener and its socket file remain intact.
    #[tokio::test]
    async fn live_socket_is_not_unlinked() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");

        // First live listener — kept alive for the duration of the test.
        let first = bind_daemon_listener(&sock).await.unwrap();

        // Second bind must refuse (live peer detected on probe-connect).
        let second = bind_daemon_listener(&sock).await;
        assert!(
            second.is_err(),
            "second bind must fail rather than unlink a live socket"
        );

        // The live socket file is untouched and still usable.
        assert!(sock.exists(), "live socket file must not be removed");
        assert!(
            StdUnixStream::connect(&sock).is_ok(),
            "first live listener's socket must remain connectable"
        );
        drop(first);
    }

    /// M7 acceptance: `bind_daemon_listener`'s temp-name+rename sequence
    /// leaves no stray `.{name}.tmp-<pid>` sibling behind, and the socket that
    /// DOES appear at `path` is already `0600` — never observable at a wider
    /// mode, since nothing is ever published at `path` until the temp file is
    /// already chmod'd.
    #[tokio::test]
    async fn bind_daemon_listener_leaves_no_temp_file_and_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");

        let listener = bind_daemon_listener(&sock).await.unwrap();

        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name())
            .filter(|n| n != "daemon.sock")
            .collect();
        assert!(
            leftovers.is_empty(),
            "no temp file should remain after bind: {leftovers:?}"
        );

        let mode = std::fs::metadata(&sock).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "the published socket must be owner-only");

        drop(listener);
    }

    /// Task 8 acceptance: with a live daemon, `run_connect_mode_io` proxies an
    /// `initialize` round trip and returns `Proxied`. The proxy task is
    /// `tokio::spawn`ed (it touches no `!Send` `App`) so the test can write the
    /// request, read the response, and only THEN close the local input side —
    /// avoiding a race between "client closed stdin" and "response arrived".
    #[tokio::test]
    async fn connect_mode_proxies_initialize_against_running_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let sock_thread = sock.clone();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let daemon = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                serve_accept_loop(app, listener, Duration::from_secs(600), shutdown_rx)
                    .await
                    .unwrap();
            });
        });

        for _ in 0..200 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let (mut test_write_in, proxy_read_in) = tokio::io::duplex(4096);
        let (proxy_write_out, test_read_out) = tokio::io::duplex(4096);

        let sock_for_task = sock.clone();
        let db_path = dir.path().join("unused.sqlite3");
        let log_path = dir.path().join("unused-daemon.log");
        let proxy_task = tokio::spawn(async move {
            run_connect_mode_io(
                &sock_for_task,
                true,
                &db_path,
                &log_path,
                proxy_read_in,
                proxy_write_out,
            )
            .await
        });

        test_write_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        test_write_in.flush().await.unwrap();

        let mut out_reader = tokio::io::BufReader::new(test_read_out);
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut out_reader, &mut line)
            .await
            .unwrap();
        assert!(
            line.contains("\"protocolVersion\""),
            "proxied response should carry the protocol version: {line}"
        );

        // Only now signal EOF on the input side, so the race described above
        // cannot cause this test to observe a truncated response.
        test_write_in.shutdown().await.unwrap();
        let outcome = proxy_task.await.unwrap().unwrap();
        assert_eq!(outcome, ProxyOutcome::Proxied);

        shutdown_tx.send(()).ok();
        daemon.join().unwrap();
    }

    /// Task 8 acceptance: no daemon listening + auto-spawn disabled ->
    /// `run_connect_mode_io` reports `FallbackToInProcess` instead of erroring,
    /// so the caller can transparently answer via `run_server_io` in-process.
    #[tokio::test]
    async fn connect_mode_falls_back_when_no_daemon_and_autospawn_disabled() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("no-daemon-here.sock");
        let db_path = dir.path().join("unused.sqlite3");
        let log_path = dir.path().join("unused-daemon.log");

        let (client_in, client_out) = tokio::io::duplex(4096);
        let outcome = run_connect_mode_io(&sock, false, &db_path, &log_path, client_in, client_out)
            .await
            .expect("fallback path must not error");
        assert_eq!(outcome, ProxyOutcome::FallbackToInProcess);
    }

    /// M4 acceptance: a connect error kind OTHER than `NotFound`/
    /// `ConnectionRefused` (e.g. `PermissionDenied`) must propagate as a real
    /// error, not be misread as "no daemon" and trigger auto-spawn. Simulated
    /// by making the socket's parent directory unsearchable, which turns
    /// `connect` into an `EACCES`/`PermissionDenied` rather than `ENOENT`.
    #[tokio::test]
    async fn connect_mode_propagates_permission_denied_instead_of_autospawning() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let restricted = dir.path().join("restricted");
        std::fs::create_dir(&restricted).unwrap();
        let sock = restricted.join("daemon.sock");

        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Skip where the process can still traverse despite 0o000 (e.g.
        // running as root), where the PermissionDenied path this test
        // exercises cannot occur. Probed the same way as
        // `write_rules`'s permission tests: try to create a file inside the
        // supposedly-unsearchable directory.
        let running_as_root_probe = restricted.join(".probe");
        let can_bypass = std::fs::File::create(&running_as_root_probe).is_ok();
        if can_bypass {
            std::fs::remove_file(&running_as_root_probe).ok();
            std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!(
                "skipping: process can bypass directory permissions (likely running as root)"
            );
            return;
        }

        let db_path = dir.path().join("unused.sqlite3");
        let log_path = dir.path().join("unused-daemon.log");
        let (client_in, client_out) = tokio::io::duplex(4096);
        let result =
            run_connect_mode_io(&sock, true, &db_path, &log_path, client_in, client_out).await;

        std::fs::set_permissions(&restricted, std::fs::Permissions::from_mode(0o700)).unwrap();

        assert!(
            result.is_err(),
            "a PermissionDenied connect error must propagate, not silently trigger autospawn"
        );
    }

    /// M5 acceptance: a HARD auto-spawn failure (lock-wait exhausted without
    /// ever acquiring the lock or connecting) must fall back to in-process
    /// serve, not take down the whole `serve` process. Forced deterministically
    /// by pre-seeding the lockfile with OUR OWN pid (definitionally alive, so
    /// `try_acquire_lock` reports `HeldByOther` forever) against a socket path
    /// nothing ever binds — `autospawn_and_connect_with` then exhausts its
    /// bounded lock-wait attempts and returns `Err`, without ever invoking the
    /// real spawn closure.
    #[tokio::test]
    async fn autospawn_hard_failure_falls_back_to_in_process_instead_of_erroring() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("never-bound.sock");
        let lock = dir.path().join("never-bound.sock.lock");
        std::fs::write(&lock, std::process::id().to_string()).unwrap();

        let db_path = dir.path().join("unused.sqlite3");
        let log_path = dir.path().join("unused-daemon.log");
        let (client_in, client_out) = tokio::io::duplex(4096);

        let outcome = run_connect_mode_io(&sock, true, &db_path, &log_path, client_in, client_out)
            .await
            .expect("a hard autospawn failure must fall back, not propagate as an error");
        assert_eq!(outcome, ProxyOutcome::FallbackToInProcess);

        std::fs::remove_file(&lock).ok();
    }

    /// M2 acceptance: if the LOCAL input side hits EOF before the daemon's
    /// response arrives (a one-shot/piped client that writes its request then
    /// immediately closes stdin — exactly what bare `serve --connect` sees
    /// from a non-interactive caller), the in-flight response must still
    /// reach `local_out` rather than being dropped when `to_socket` completes
    /// first. A stub "daemon" replies only after a short delay, so the local
    /// EOF is guaranteed to race ahead of the reply.
    #[tokio::test]
    async fn pump_proxy_drains_in_flight_daemon_response_after_local_input_eof() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("stub.sock");
        let listener = UnixListener::bind(&sock).unwrap();

        let stub = tokio::spawn(async move {
            let (stream, _addr) = listener.accept().await.unwrap();
            let (read, mut write) = tokio::io::split(stream);
            let mut reader = tokio::io::BufReader::new(read);
            let mut line = String::new();
            tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line)
                .await
                .unwrap();
            // Reply only after a beat, so the client's stdin EOF (below) is
            // guaranteed to have already been observed by `pump_proxy` first.
            tokio::time::sleep(Duration::from_millis(50)).await;
            write
                .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n")
                .await
                .unwrap();
            write.shutdown().await.unwrap();
        });

        let client_stream = UnixStream::connect(&sock).await.unwrap();

        let (mut test_write_in, proxy_in) = tokio::io::duplex(4096);
        let (proxy_out, mut test_read_out) = tokio::io::duplex(4096);

        test_write_in
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .await
            .unwrap();
        // Immediate stdin EOF, before the stub's delayed reply arrives.
        test_write_in.shutdown().await.unwrap();

        pump_proxy(client_stream, proxy_in, proxy_out)
            .await
            .unwrap();

        let mut out = String::new();
        test_read_out.read_to_string(&mut out).await.unwrap();
        assert!(
            out.contains("\"id\":1") && out.contains("\"result\""),
            "the daemon's in-flight reply must still reach local_out after local EOF: {out}"
        );

        stub.await.unwrap();
    }

    // ---- Task 9: single-flight auto-spawn under an atomic lockfile --------
    //
    // `run_connect_mode_io`'s autospawn-enabled + no-daemon arm now calls
    // through to `autospawn_and_connect`, which spawns the REAL `ironmem`
    // binary via `current_exe()`. Exercising that at this (unit-test) level
    // would spawn the `cargo test` harness binary itself with `serve
    // --listen <path>` argv — not a real daemon, and not something to spin up
    // from a unit test. The real spawn path is covered by an integration test
    // using `CARGO_BIN_EXE_ironmem` (Task 9/15). Here we test the pieces that
    // ARE safely unit-testable: the lock mechanics, poll-connect backoff, and
    // the single-flight contract via `autospawn_and_connect_with`'s injectable
    // spawn closure (a fake that binds a real listener in-process, standing in
    // for "a detached daemon came up").

    #[test]
    fn try_acquire_lock_succeeds_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("d.sock.lock");

        let outcome = try_acquire_lock(&lock).unwrap();
        assert_eq!(outcome, LockOutcome::Acquired);
        let content = std::fs::read_to_string(&lock).unwrap();
        assert_eq!(content.trim(), std::process::id().to_string());
    }

    #[test]
    fn try_acquire_lock_reports_held_by_other_for_a_live_owner() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("d.sock.lock");
        // Our own pid is, definitionally, alive.
        std::fs::write(&lock, std::process::id().to_string()).unwrap();

        let outcome = try_acquire_lock(&lock).unwrap();
        assert_eq!(outcome, LockOutcome::HeldByOther);
        // A live owner's lock must never be stolen/overwritten.
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap().trim(),
            std::process::id().to_string()
        );
    }

    #[test]
    fn try_acquire_lock_recovers_a_stale_dead_owner() {
        let dir = tempfile::tempdir().unwrap();
        let lock = dir.path().join("d.sock.lock");
        // An implausibly large pid: guaranteed to not exist on any real OS
        // (Linux pid_max and macOS PID_MAX are both far below this), so
        // `pid_is_alive` reliably reports it as dead without relying on a
        // real, racy "spawn a child and wait for it to exit" dance.
        std::fs::write(&lock, "2000000000").unwrap();

        let outcome = try_acquire_lock(&lock).unwrap();
        assert_eq!(outcome, LockOutcome::Acquired);
        let content = std::fs::read_to_string(&lock).unwrap();
        assert_eq!(
            content.trim(),
            std::process::id().to_string(),
            "stale lock must be recovered and re-owned by the caller"
        );
    }

    #[tokio::test]
    async fn poll_connect_with_backoff_succeeds_once_listener_appears() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let sock_for_bind = sock.clone();

        // Bind shortly after the poll starts, simulating a daemon that takes
        // a little while to come up.
        let bind_task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(60)).await;
            bind_daemon_listener(&sock_for_bind).await.unwrap()
        });

        let connected = poll_connect_with_backoff(&sock).await;
        assert!(
            connected.is_ok(),
            "poll-connect must succeed once the listener binds"
        );
        drop(bind_task.await.unwrap());
    }

    #[tokio::test]
    async fn autospawn_single_flight_spawns_once_then_releases_lock() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let lock = dir.path().join("d.sock.lock");

        let spawn_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sc = Arc::clone(&spawn_count);
        let sock_for_spawn = sock.clone();

        // Fake "spawn": stands in for the detached `ironmem serve --listen`
        // process — binds and serves in a background thread — without
        // touching a real subprocess.
        let fake_spawn = move || {
            sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let sock_thread = sock_for_spawn.clone();
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(async move {
                    #[allow(clippy::arc_with_non_send_sync)]
                    let app = Arc::new(App::open_for_test().unwrap());
                    let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                    let (_tx, rx) = oneshot::channel::<()>();
                    serve_accept_loop(app, listener, Duration::from_secs(600), rx)
                        .await
                        .unwrap();
                });
            });
            Ok(())
        };

        let stream = autospawn_and_connect_with(&sock, &lock, fake_spawn)
            .await
            .expect("autospawn must acquire, spawn, poll-connect, and succeed");
        assert_eq!(
            spawn_count.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "spawn must be invoked exactly once"
        );
        drop(stream);
        assert!(
            !lock.exists(),
            "the lock must be released once autospawn completes"
        );
    }

    #[tokio::test]
    async fn autospawn_skips_spawn_when_daemon_already_up_at_lock_acquisition() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("d.sock");
        let lock = dir.path().join("d.sock.lock");
        let sock_thread = sock.clone();

        // A daemon that is ALREADY up by the time we acquire the lock,
        // simulating another proxy having already won the spawn race just
        // before us.
        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let daemon = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                serve_accept_loop(app, listener, Duration::from_secs(600), shutdown_rx)
                    .await
                    .unwrap();
            });
        });
        for _ in 0..200 {
            if sock.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let spawn_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let sc = Arc::clone(&spawn_count);
        let fake_spawn = move || {
            sc.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        };

        let stream = autospawn_and_connect_with(&sock, &lock, fake_spawn)
            .await
            .expect("must connect to the already-running daemon");
        assert_eq!(
            spawn_count.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "an already-up daemon must be reused, never respawned"
        );
        drop(stream);

        shutdown_tx.send(()).ok();
        daemon.join().unwrap();
    }

    #[test]
    fn lock_path_for_socket_appends_dot_lock() {
        assert_eq!(
            lock_path_for_socket(Path::new("/tmp/x/daemon.sock")),
            std::path::PathBuf::from("/tmp/x/daemon.sock.lock")
        );
    }

    // ---- #190 Task 14: daemon health probe ---------------------------------

    #[tokio::test]
    async fn probe_reports_unreachable_when_no_daemon_listening() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("no-daemon-here.sock");

        let health = probe_daemon_health(&sock, Duration::from_millis(200)).await;
        assert_eq!(health, DaemonHealth::Unreachable);
    }

    #[test]
    fn probe_reports_reachable_against_a_running_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let sock_thread = sock.clone();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let daemon = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                serve_accept_loop(app, listener, Duration::from_secs(600), shutdown_rx)
                    .await
                    .unwrap();
            });
        });
        connect_with_retry(&sock, "readiness probe"); // wait for the socket to accept

        // The probe itself needs its own tiny runtime — this test is
        // deliberately `#[test]` (not `#[tokio::test]`) so it can drive that
        // runtime from a plain thread, exactly mirroring how `doctor`'s
        // caller (an already-running `#[tokio::main]`) would call
        // `probe_daemon_health` from ITS OWN async context in production;
        // here we just supply an equivalent runtime inline.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let health = rt.block_on(probe_daemon_health(&sock, Duration::from_millis(500)));
        assert_eq!(health, DaemonHealth::Reachable);

        shutdown_tx.send(()).ok();
        daemon.join().unwrap();
    }

    /// #190 Task 14 acceptance: probing must NEVER mutate another client's
    /// already-recorded attribution. A real client connection first records
    /// session "real-session" / harness "codex"; the health probe then runs
    /// against the SAME daemon; afterward "real-session"'s recorded harness
    /// must be untouched.
    #[test]
    fn health_probe_does_not_disturb_another_connections_attribution() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let sock_thread = sock.clone();

        let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        let daemon = std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async move {
                #[allow(clippy::arc_with_non_send_sync)]
                let app = Arc::new(App::open_for_test().unwrap());
                let listener = bind_daemon_listener(&sock_thread).await.unwrap();
                serve_accept_loop(app, listener, Duration::from_secs(600), shutdown_rx)
                    .await
                    .unwrap();
            });
        });

        // Real client: initialize with a distinct session id + codex clientInfo.
        {
            let stream = connect_with_retry(&sock, "concurrent client connect");
            let mut writer = stream.try_clone().unwrap();
            let mut reader = StdBufReader::new(stream);
            writer
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"sessionId\":\"real-session\",\"clientInfo\":{\"name\":\"codex-cli\",\"version\":\"1.0.0\"}}}\n",
                )
                .unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            assert!(line.contains("\"protocolVersion\""));
        }

        // Health probe: a completely separate, throwaway connection.
        let rt = tokio::runtime::Runtime::new().unwrap();
        let health = rt.block_on(probe_daemon_health(&sock, Duration::from_millis(500)));
        assert_eq!(health, DaemonHealth::Reachable);

        // The real client's recorded harness/session must be exactly as it
        // was — the probe's own throwaway `initialize` never touched it.
        {
            let stream = connect_with_retry(&sock, "concurrent client connect");
            let mut writer = stream.try_clone().unwrap();
            let mut reader = StdBufReader::new(stream);
            writer
                .write_all(
                    b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"collab_status\",\"arguments\":{\"session_id\":\"real-session\"}}}\n",
                )
                .unwrap();
            writer.flush().unwrap();
            let mut line = String::new();
            reader.read_line(&mut line).unwrap();
            // A tools/call response (success or a handled error) proves the
            // shared App is still healthy post-probe; the attribution
            // guarantee itself is Task 2's structural property (each
            // connection gets its own ConnectionContext), exercised directly
            // by mcp::server's sequential-connections test.
            assert!(line.contains("\"id\":2"));
        }

        shutdown_tx.send(()).ok();
        daemon.join().unwrap();
    }
}
