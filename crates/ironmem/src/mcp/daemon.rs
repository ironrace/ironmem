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
/// until `shutdown` fires. Owns the single `Arc<App>`. Every accepted connection
/// becomes a `!Send` handler future (it clones `Arc<App>` — cheap, same thread,
/// never moved) that reuses `run_server_io` over the `UnixStream` halves. All
/// handlers are polled cooperatively on THIS thread via a `FuturesUnordered`,
/// alongside the accept branch, under one `tokio::select!` — see the module doc
/// for why this (not `spawn_local`) is the confinement mechanism.
///
/// MUST be driven on the `block_on` thread of a multi-thread runtime and NOT
/// inside a `LocalSet`, so the `block_in_place` inside `run_server_io` is valid.
///
/// Accept errors are logged and the loop continues: a transient accept failure
/// (a client that vanished mid-handshake, a momentary fd-limit hiccup) must not
/// tear down a daemon serving many other peers. On `shutdown` we stop accepting
/// and return immediately; in-flight handlers are dropped (their sockets close),
/// which is the intended behavior for both idle-timeout (Task 7) and process
/// exit.
#[cfg(unix)]
pub async fn serve_accept_loop(
    app: Arc<App>,
    listener: UnixListener,
    mut shutdown: oneshot::Receiver<()>,
) -> Result<(), MemoryError> {
    use futures_util::stream::FuturesUnordered;
    use futures_util::StreamExt;

    // Connection handler futures, all `!Send`, polled on this single thread.
    let mut connections = FuturesUnordered::new();

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _addr)) => {
                        let app_conn = Arc::clone(&app);
                        connections.push(async move {
                            let (read, write) = tokio::io::split(stream);
                            let reader = tokio::io::BufReader::new(read);
                            if let Err(e) =
                                super::server::run_server_io(app_conn, reader, write).await
                            {
                                tracing::warn!("daemon connection ended with error: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::warn!("daemon accept error (continuing): {e}"),
                }
            }
            // Drive in-flight connection handlers to completion. `None` (empty
            // set) simply means no connections are pending; the branch resolves
            // immediately, so we guard with `!connections.is_empty()` to avoid a
            // busy-loop and let `select!` fall through to the accept/shutdown
            // branches instead.
            Some(()) = connections.next(), if !connections.is_empty() => {}
            _ = &mut shutdown => break,
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
/// the process is killed. Task 7 wires idle-timeout shutdown onto this same
/// `serve_accept_loop` signal.
#[cfg(unix)]
pub fn run_daemon(
    config: crate::config::Config,
    socket_path: std::path::PathBuf,
) -> Result<(), MemoryError> {
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

        let listener = bind_daemon_listener(&socket_path).await?;
        // Never-fired shutdown: run until process exit. Task 7 replaces this.
        let (_tx, rx) = oneshot::channel::<()>();
        serve_accept_loop(app, listener, rx).await
    })
}

#[cfg(all(test, unix))]
mod daemon_tests {
    use super::*;
    use std::io::{BufRead, BufReader as StdBufReader, Write};
    use std::os::unix::net::UnixStream as StdUnixStream;
    use std::time::Duration;

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
                serve_accept_loop(app, listener, shutdown_rx).await.unwrap();
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
