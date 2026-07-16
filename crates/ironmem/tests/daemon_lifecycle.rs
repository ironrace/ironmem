//! Task 16 (#190): end-to-end shared-daemon lifecycle coverage, driven
//! entirely through the real compiled `ironmem` binary (`CARGO_BIN_EXE_ironmem`),
//! never the in-process unit-test seams.
//!
//! Three scenarios make up this consolidated lifecycle coverage:
//! 1. M-proxy auto-spawn race (exactly one daemon spawns, all share one DB) —
//!    covered by `daemon_autospawn_race.rs`'s
//!    `concurrent_proxies_single_flight_one_daemon_and_share_one_db`, which
//!    ALSO already verifies the daemon self-terminates via its idle timer
//!    afterward. Not duplicated here.
//! 2. Idle-timeout boundary: a connection admitted while the idle timer is
//!    counting down must be served, not dropped — see
//!    `admitted_connection_near_idle_boundary_is_served_not_dropped` below.
//! 3. No-daemon + `--no-autospawn` fallback answering `initialize` via
//!    in-process `serve` — see `no_daemon_no_autospawn_falls_back_to_in_process_serve`
//!    below.
//!
//! All three run with test-overridden idle windows / tempdir socket paths and
//! leave no orphaned daemon process, socket, or lockfile behind.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use serde_json::json;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

fn connect_with_retry(sock: &Path) -> UnixStream {
    for _ in 0..300 {
        if let Ok(stream) = UnixStream::connect(sock) {
            return stream;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!(
        "could not connect to daemon socket {} within timeout",
        sock.display()
    );
}

/// Kill a spawned child on drop, so a failing assertion still cleans up
/// rather than leaking a daemon process.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn base_env(cmd: &mut Command, home: &Path, db_path: &Path) {
    cmd.env("HOME", home)
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_MCP_MODE", "trusted")
        .env("IRONMEM_AUTO_BOOTSTRAP", "0");
}

/// Run one `initialize` request through a piped-stdio child, returning the
/// response line. Reads the response BEFORE closing stdin (dropping the
/// returned sender), avoiding the "client closed stdin before the response
/// arrived" race a raw byte-pump proxy is otherwise subject to (see
/// `mcp::daemon`'s `connect_mode_proxies_initialize_against_running_daemon`
/// for the same hazard at unit-test granularity).
fn run_initialize_over_stdio(mut cmd: Command) -> (Child, String) {
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("child must spawn");
    let mut stdin = child.stdin.take().unwrap();
    let stdout = child.stdout.take().unwrap();
    let mut reader = BufReader::new(stdout);

    writeln!(
        stdin,
        "{}",
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
    )
    .unwrap();
    stdin.flush().unwrap();

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    drop(stdin); // EOF now, only after the response is already in hand.
    (child, line)
}

/// #190 Task 16 scenario 2: a connection admitted WHILE the daemon's idle
/// timer is counting down must be served, not dropped in favor of shutdown.
/// Exercises the same contract as `mcp::daemon`'s
/// `new_connection_resets_idle_timer_and_is_served` unit test, but through
/// the real `ironmem serve --listen` / `serve --connect` binary end to end.
#[test]
fn admitted_connection_near_idle_boundary_is_served_not_dropped() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let sock = temp.path().join("daemon.sock");
    std::fs::create_dir_all(&home).unwrap();

    // Not razor-thin: the idle timer arms the instant the daemon starts (by
    // design), so under a heavily parallel `cargo test --workspace` run,
    // real OS-scheduling jitter for this subprocess to actually bind and for
    // our own `connect_with_retry` to land could eat into a very short
    // window. 6s keeps the boundary meaningfully "near" while giving that
    // jitter comfortable headroom.
    let idle_secs = 6;
    let mut listen_cmd = Command::new(bin());
    listen_cmd
        .arg("serve")
        .arg("--listen")
        .arg(&sock)
        .env("IRONMEM_DAEMON_IDLE_SECS", idle_secs.to_string());
    base_env(&mut listen_cmd, &home, &db_path);
    let daemon = KillOnDrop(
        listen_cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon must spawn"),
    );
    connect_with_retry(&sock);

    // Connection 1: connect + initialize + disconnect immediately, arming
    // the idle timer.
    {
        let stream = connect_with_retry(&sock);
        let mut writer = stream.try_clone().unwrap();
        let mut reader = BufReader::new(stream);
        writer
            .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
            .unwrap();
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(line.contains("\"protocolVersion\""));
    }

    // Wait well past half the idle window, then connect again via the REAL
    // `--connect` proxy binary. This must succeed — proving the connection
    // was admitted (and the timer disarmed) rather than the daemon having
    // already exited out from under it.
    std::thread::sleep(Duration::from_millis(idle_secs * 1000 / 2));
    let mut connect_cmd = Command::new(bin());
    connect_cmd.arg("serve").arg("--connect").arg(&sock);
    base_env(&mut connect_cmd, &home, &db_path);
    let (mut proxy_child, line) = run_initialize_over_stdio(connect_cmd);
    assert!(
        line.contains("\"protocolVersion\""),
        "the near-boundary connection must be served, not dropped: {line}"
    );
    let status = proxy_child.wait().unwrap();
    assert!(status.success(), "proxy must exit cleanly: {status:?}");

    // Now let this (reset) idle window fully elapse with no further
    // activity: the daemon must shut down on its own, cleaning up the
    // socket and lockfile.
    let lock = temp.path().join("daemon.sock.lock");
    for _ in 0..100 {
        if !sock.exists() && !lock.exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(
        !sock.exists(),
        "daemon must remove its socket after the reset idle window elapses"
    );
    assert!(!lock.exists(), "daemon must remove its lockfile too");
    // Explicit drop: the process already exited on its own (just asserted
    // above via the socket/lock removal), so this is a harmless no-op kill
    // of an already-dead pid — it only matters as a safety net if the
    // assertions above failed instead of panicking past this point.
    drop(daemon);
}

/// #190 Task 16 scenario 3: no daemon reachable + `--no-autospawn` set ->
/// `serve --connect` must transparently answer `initialize` via in-process
/// stdio `serve`, WITHOUT ever creating a socket, lockfile, or daemon
/// process.
#[test]
fn no_daemon_no_autospawn_falls_back_to_in_process_serve() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let sock = temp.path().join("never-bound.sock");
    let lock = temp.path().join("never-bound.sock.lock");
    std::fs::create_dir_all(&home).unwrap();

    let mut cmd = Command::new(bin());
    cmd.arg("serve")
        .arg("--connect")
        .arg(&sock)
        .arg("--no-autospawn");
    base_env(&mut cmd, &home, &db_path);
    let (mut child, line) = run_initialize_over_stdio(cmd);

    assert!(
        line.contains("\"protocolVersion\""),
        "fallback must still answer initialize: {line}"
    );
    let status = child.wait().unwrap();
    assert!(status.success(), "fallback process must exit cleanly");

    assert!(
        !sock.exists(),
        "no daemon should ever have been spawned or bound"
    );
    assert!(!lock.exists(), "no lockfile should ever have been created");
}
