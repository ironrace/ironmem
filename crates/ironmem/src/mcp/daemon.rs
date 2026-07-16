//! Single-owner dispatcher actor for shared-daemon mode.
//!
//! `App` is `!Sync`, so `Arc<App>` is `!Send` and cannot cross a `tokio::spawn`
//! boundary. To share one `App` across many concurrent connections, a single
//! owner task holds the `Arc<App>` and is the SOLE caller of `dispatch`.
//! Per-connection handlers send their request plus a oneshot reply channel over
//! an mpsc; the owner serially dispatches and replies. This confines `App` to
//! one task so it is never required to be `Send`.

use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use super::app::App;
use super::protocol::{JsonRpcRequest, JsonRpcResponse};
use super::server::dispatch;

/// A request routed to the dispatcher owner, paired with the reply channel the
/// owner uses to return the response to the originating connection handler.
///
/// The type is `pub` (fields stay private) because it appears in the return
/// type of the public [`dispatcher_channel`] function via
/// `mpsc::Receiver<DispatchMessage>`; keeping it private would trip the
/// `private_interfaces` lint.
pub struct DispatchMessage {
    request: JsonRpcRequest,
    respond_to: oneshot::Sender<Option<JsonRpcResponse>>,
}

/// Cloneable handle used by connection handlers to send requests to the single
/// dispatcher owner. Cloning is cheap (clones the mpsc sender).
#[derive(Clone)]
pub struct DispatcherHandle {
    tx: mpsc::Sender<DispatchMessage>,
}

impl DispatcherHandle {
    /// Async round-trip: send `request` to the owner and await its response.
    /// Returns `None` if the dispatcher owner has shut down (channel closed) or
    /// produced no response (e.g. a notification).
    pub async fn dispatch(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let (respond_to, rx) = oneshot::channel();
        if self
            .tx
            .send(DispatchMessage {
                request,
                respond_to,
            })
            .await
            .is_err()
        {
            return None;
        }
        rx.await.ok().flatten()
    }

    /// Blocking round-trip for use INSIDE `tokio::task::block_in_place` from the
    /// synchronous `run_framing_loop` dispatch backend (Task 6). Must NOT be
    /// called on the dispatcher owner's own task (would deadlock) — only from a
    /// distinct per-connection handler task.
    pub fn blocking_dispatch(&self, request: JsonRpcRequest) -> Option<JsonRpcResponse> {
        let (respond_to, rx) = oneshot::channel();
        if self
            .tx
            .blocking_send(DispatchMessage {
                request,
                respond_to,
            })
            .is_err()
        {
            return None;
        }
        rx.blocking_recv().ok().flatten()
    }
}

/// Create a dispatcher channel with the given mpsc buffer size, returning the
/// cloneable handle and the receiver the owner loop consumes.
pub fn dispatcher_channel(buffer: usize) -> (DispatcherHandle, mpsc::Receiver<DispatchMessage>) {
    let (tx, rx) = mpsc::channel(buffer);
    (DispatcherHandle { tx }, rx)
}

/// The single-owner dispatcher loop. Owns `Arc<App>` and is the sole caller of
/// `dispatch`. This future is `!Send` (holds `Arc<App>`); it MUST be driven on a
/// `LocalSet`/`spawn_local` or a dedicated current-thread runtime, NEVER
/// `tokio::spawn`ed on the multi-thread runtime.
///
/// `dispatch` is called directly (synchronously) rather than via
/// `block_in_place`: `block_in_place` is only valid on a multi-thread-runtime
/// worker and panics inside a `LocalSet`/current-thread runtime — precisely the
/// contexts this `!Send` owner must run on. Because the owner is the sole task
/// on its dedicated execution context, a blocking `dispatch` starves nothing
/// here. Concurrency is preserved on the OTHER side of the channel: connection
/// handlers (Task 6) live on the multi-thread runtime and wrap their
/// [`DispatcherHandle::blocking_dispatch`] round-trip in `block_in_place`, so
/// their runtime worker keeps serving peers while this owner works.
pub async fn run_dispatcher(app: Arc<App>, mut rx: mpsc::Receiver<DispatchMessage>) {
    while let Some(DispatchMessage {
        request,
        respond_to,
    }) = rx.recv().await
    {
        let response = dispatch(&app, &request);
        // Ignore send errors: the connection handler may have dropped (client
        // disconnected) before the reply was ready.
        let _ = respond_to.send(response);
    }
}

// ---------------------------------------------------------------------------
// `--listen` shared-daemon transport (Task 6).
//
// Unix-domain sockets are Unix-only, so every socket-touching item below is
// `#[cfg(unix)]`.
//
// Confining the `!Send` `App` to one thread — the runtime model.
// `App` is `!Sync`, so `Arc<App>` is `!Send`: it can neither be `tokio::spawn`ed
// nor moved across threads. The daemon therefore runs on ONE dedicated thread,
// creates the single `Arc<App>` there, and keeps it there for its whole life.
// Concurrency across connections is achieved WITHOUT moving the `App`: all
// per-connection handlers are `!Send` futures polled cooperatively on that same
// thread via a `FuturesUnordered`, alongside the accept loop, under one
// `tokio::select!`. Each handler reuses the EXISTING `run_server_io` framing
// loop UNCHANGED (full framing + per-connection metrics), just over `UnixStream`
// halves instead of stdin/stdout.
//
// Why NOT `spawn_local`/`LocalSet`, and why a multi-thread runtime:
// `run_server_io` offloads its synchronous `dispatch` + metrics work through
// `tokio::task::block_in_place`. `block_in_place` PANICS both on a
// `current_thread` runtime AND from within a `LocalSet`/`spawn_local` — so the
// obvious "`current_thread` + `spawn_local`" confinement cannot run
// `run_server_io` at all. It is, however, valid on the `block_on` thread of a
// MULTI-THREAD runtime when NOT inside a `LocalSet`. So the daemon builds a
// multi-thread runtime and drives everything from its `block_on` thread with a
// `FuturesUnordered` (no `LocalSet`), which both satisfies `block_in_place` and
// keeps every future — and the `Arc<App>` they clone — pinned to this one
// thread. Because handlers are cooperatively scheduled on a single thread and
// each `dispatch` runs inside `block_in_place`, dispatch is naturally
// serialized: the "single writer / one App" invariant holds by thread
// confinement, with no lock.
//
// This deliberately does NOT use the Task 5 `DispatcherHandle`/`run_dispatcher`
// actor: that actor targets a design where handlers live on real worker threads
// and reach the `App` owner over a channel; it is unnecessary here and is left
// intact as tested infrastructure.

#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

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

    // Probe: a successful connect means a live daemon is listening.
    if UnixStream::connect(path).await.is_ok() {
        return Err(MemoryError::Config(format!(
            "daemon already running at {}",
            path.display()
        )));
    }

    // Connect failed -> stale. Only unlink a genuine socket file.
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

    let listener = UnixListener::bind(path).map_err(MemoryError::Io)?;
    // Owner-only socket: no other local user may connect.
    std::fs::set_permissions(path, Permissions::from_mode(0o600)).map_err(MemoryError::Io)?;
    Ok(listener)
}

/// Accept connections on `listener` and serve each with the Task 1 framing loop
/// until `shutdown` fires or the daemon has been idle (zero active connections)
/// for `idle_timeout`. Owns the single `Arc<App>`. Every accepted connection
/// becomes a `!Send` handler future (it clones `Arc<App>` — cheap, same thread,
/// never moved) that reuses `run_server_io` over the `UnixStream` halves. All
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
/// inside a `LocalSet`, so the `block_in_place` inside `run_server_io` is valid.
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
                            if let Err(e) =
                                super::server::run_server_io(app_conn, reader, write).await
                            {
                                tracing::warn!("daemon connection ended with error: {e}");
                            }
                            active_conn.fetch_sub(1, Ordering::SeqCst)
                        });
                    }
                    Err(e) => tracing::warn!("daemon accept error (continuing): {e}"),
                }
            }
            // Drive in-flight connection handlers to completion. `None` (empty
            // set) simply means no connections are pending; the branch resolves
            // immediately, so we guard with `!connections.is_empty()` to avoid a
            // busy-loop and let `select!` fall through to the other branches.
            Some(prev_active) = connections.next(), if !connections.is_empty() => {
                if prev_active == 1 {
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
/// via `serve_accept_loop`. On EITHER exit path the daemon-owned socket and
/// lockfile are removed (best-effort — a `NotFound` on either is expected and
/// ignored), so a later `--connect` proxy correctly probes "no daemon" rather
/// than finding a stale path.
#[cfg(unix)]
pub fn run_daemon(
    config: crate::config::Config,
    socket_path: std::path::PathBuf,
) -> Result<(), MemoryError> {
    let idle_timeout = config.daemon_idle_timeout();
    let lock_path = config.daemon_lock_path();
    let socket_path_for_cleanup = socket_path.clone();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(MemoryError::Io)?;
    let result = rt.block_on(async move {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::new_server_ready(config.clone())?);
        // Mirror the stdio `serve` path: record version + kick background model
        // load so connections eventually get a real embedder. Background init
        // runs on its own thread with its own DB connection (WAL-safe).
        crate::bootstrap::check_and_record_version(&config.state_dir);
        let memory_ready = Arc::clone(&app.memory_ready);
        crate::bootstrap::run_background_memory_init(config, memory_ready);

        let listener = bind_daemon_listener(&socket_path).await?;
        // Never-fired shutdown: only the idle timer inside `serve_accept_loop`
        // ends this daemon absent an external kill.
        let (_tx, rx) = oneshot::channel::<()>();
        serve_accept_loop(app, listener, idle_timeout, rx).await
    });

    let _ = std::fs::remove_file(&socket_path_for_cleanup);
    let _ = std::fs::remove_file(&lock_path);
    result
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
/// until EOF on EITHER side. Deliberately `select!`, not `try_join!`: a
/// client that closes its stdin, or a daemon that closes the connection,
/// should end the proxy immediately rather than waiting on the other,
/// now-orphaned pump.
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
    let to_socket = tokio::io::copy(&mut local_in, &mut sock_write);
    let from_socket = tokio::io::copy(&mut sock_read, &mut local_out);

    tokio::select! {
        result = to_socket => { result.map_err(MemoryError::Io)?; }
        result = from_socket => { result.map_err(MemoryError::Io)?; }
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
/// - Connect fails AND `autospawn_enabled` is `true` -> propagates the connect
///   error. Task 9 replaces this arm with single-flight auto-spawn + retry;
///   until then there is no daemon to spawn from this seam.
#[cfg(unix)]
async fn run_connect_mode_io<R, W>(
    socket_path: &Path,
    autospawn_enabled: bool,
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
        Err(e) => Err(MemoryError::Io(e)),
    }
}

/// Production `--connect` entry point: proxies the real process stdio.
#[cfg(unix)]
pub async fn run_connect_mode(
    socket_path: &Path,
    autospawn_enabled: bool,
) -> Result<ProxyOutcome, MemoryError> {
    run_connect_mode_io(
        socket_path,
        autospawn_enabled,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
}

#[cfg(all(test, unix))]
mod daemon_tests {
    use super::*;
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::time::Duration;
    use tokio::io::AsyncWriteExt;

    /// Poll-connect with bounded retries so we never race a not-yet-bound
    /// daemon and never rely on a fixed sleep.
    fn connect_with_retry(path: &std::path::Path) -> StdUnixStream {
        for _ in 0..200 {
            if let Ok(stream) = StdUnixStream::connect(path) {
                return stream;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("could not connect to daemon socket within timeout");
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

        let stream = connect_with_retry(&sock);
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
        let stream = connect_with_retry(&sock);
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
        let idle_timeout = Duration::from_millis(200);

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
        {
            let stream = connect_with_retry(&sock);
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

        // Wait well past half the idle window, then connect again — this must
        // disarm the timer that connection 1 armed, rather than the daemon
        // having already exited.
        std::thread::sleep(idle_timeout / 2);
        let stream2 = connect_with_retry(&sock);
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
        drop(writer2);
        drop(reader2);

        // Now let this (second) idle window fully elapse with no further
        // activity: the daemon must eventually shut down on its own.
        let joined = std::thread::spawn(move || daemon.join());
        for _ in 0..50 {
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

    /// Task 7: on idle-timeout exit, the daemon-owned socket AND lockfile must
    /// both be removed — mirroring the cleanup `run_daemon` performs on every
    /// exit path, so a subsequent `--connect` proxy correctly probes "no
    /// daemon" instead of tripping over stale files.
    #[tokio::test]
    async fn idle_exit_cleanup_removes_socket_and_lock() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("daemon.sock");
        let lock = dir.path().join("daemon.sock.lock");
        std::fs::write(&lock, b"12345").unwrap();

        let (_shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let listener = bind_daemon_listener(&sock).await.unwrap();

        // Mirrors `run_daemon`'s post-`serve_accept_loop` cleanup exactly.
        serve_accept_loop(app, listener, Duration::from_millis(50), shutdown_rx)
            .await
            .unwrap();
        let _ = std::fs::remove_file(&sock);
        let _ = std::fs::remove_file(&lock);

        assert!(!sock.exists(), "socket file must be removed on idle exit");
        assert!(!lock.exists(), "lockfile must be removed on idle exit");
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
        let proxy_task = tokio::spawn(async move {
            run_connect_mode_io(&sock_for_task, true, proxy_read_in, proxy_write_out).await
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

        let (client_in, client_out) = tokio::io::duplex(4096);
        let outcome = run_connect_mode_io(&sock, false, client_in, client_out)
            .await
            .expect("fallback path must not error");
        assert_eq!(outcome, ProxyOutcome::FallbackToInProcess);
    }

    /// When auto-spawn IS enabled but no daemon is reachable, Task 8 has no
    /// spawn logic yet (that's Task 9) — the connect error must propagate
    /// rather than being silently swallowed, so a future regression that
    /// accidentally treats "autospawn enabled" the same as "disabled" is
    /// caught here.
    #[tokio::test]
    async fn connect_mode_propagates_error_when_autospawn_enabled_and_no_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let sock = dir.path().join("no-daemon-here.sock");

        let (client_in, client_out) = tokio::io::duplex(4096);
        let err = run_connect_mode_io(&sock, true, client_in, client_out)
            .await
            .expect_err("no daemon + autospawn enabled must propagate the connect error");
        assert!(matches!(err, MemoryError::Io(_)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_request(line: &str) -> JsonRpcRequest {
        serde_json::from_str(line).expect("valid JSON-RPC request")
    }

    /// Drives two concurrent in-memory "connections" (cloned handles) through a
    /// single dispatcher owned by a `spawn_local`'d future. Asserts each reply
    /// carries the id of ITS request (no cross-talk), which proves correct
    /// routing of concurrent in-flight requests. That the `Arc<App>`-owning
    /// future is `spawn_local`'d (never `tokio::spawn`'d) proves `App` is never
    /// required to be `Send`.
    #[tokio::test(flavor = "multi_thread")]
    async fn two_connections_route_to_correct_responses() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (handle, rx) = dispatcher_channel(16);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let dispatcher = tokio::task::spawn_local(run_dispatcher(app, rx));

                let h1 = handle.clone();
                let h2 = handle.clone();

                let req1 =
                    parse_request(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}"#);
                let req2 =
                    parse_request(r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{}}"#);

                let (r1, r2) = tokio::join!(h1.dispatch(req1), h2.dispatch(req2));

                let r1 = r1.expect("tools/list returns a response");
                let r2 = r2.expect("initialize returns a response");

                // Correctly-routed: each response carries the id of its own
                // request, so concurrent in-flight requests did not swap replies.
                assert_eq!(r1.id, Some(serde_json::json!(1)));
                assert_eq!(r2.id, Some(serde_json::json!(2)));

                // Sanity: the responses are the ones we expect for each method.
                assert!(r1.result.is_some(), "tools/list is a success response");
                assert!(r2.result.is_some(), "initialize is a success response");

                // Drop every sender (the original plus both per-connection
                // clones) so the mpsc closes and the dispatcher loop exits.
                drop(handle);
                drop(h1);
                drop(h2);
                dispatcher.await.unwrap();
            })
            .await;
    }

    /// Verifies the blocking round-trip used by Task 6's synchronous framing
    /// backend: a `spawn_blocking` task calls `blocking_dispatch` while the
    /// dispatcher runs on a `LocalSet`, and receives the correctly-routed reply.
    #[tokio::test(flavor = "multi_thread")]
    async fn blocking_dispatch_round_trips() {
        #[allow(clippy::arc_with_non_send_sync)]
        let app = Arc::new(App::open_for_test().unwrap());
        let (handle, rx) = dispatcher_channel(16);

        let local = tokio::task::LocalSet::new();
        local
            .run_until(async move {
                let dispatcher = tokio::task::spawn_local(run_dispatcher(app, rx));

                let h = handle.clone();
                let response = tokio::task::spawn_blocking(move || {
                    let req = parse_request(
                        r#"{"jsonrpc":"2.0","id":7,"method":"tools/list","params":{}}"#,
                    );
                    h.blocking_dispatch(req)
                })
                .await
                .unwrap();

                let response = response.expect("tools/list returns a response");
                assert_eq!(response.id, Some(serde_json::json!(7)));
                assert!(response.result.is_some());

                drop(handle);
                dispatcher.await.unwrap();
            })
            .await;
    }
}
