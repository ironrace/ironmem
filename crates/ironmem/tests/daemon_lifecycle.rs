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
//! 4. Graceful retire on `SIGTERM` (the install-upgrade path): a signalled
//!    daemon stops admitting new clients but serves its attached ones to the
//!    end — see `sigterm_retires_the_daemon_without_cutting_off_an_attached_client`
//!    below. This is the only test that exercises the real signal wiring in
//!    `run_daemon`; the unit tests fire the shutdown channel directly and so
//!    cannot see whether anything is actually listening for a signal.
//!
//! All run with test-overridden idle windows / tempdir socket paths and leave
//! no orphaned daemon process, socket, or lockfile behind.
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

/// Bound on how long a purely causal, event-driven wait may take before we
/// call it a hang. NOT a timing margin: every step below is triggered by an
/// event this test just caused (a signal, a write, a close), so a correct
/// daemon reaches each observable in milliseconds and only a real regression
/// ever spends this budget. Generous because it also has to absorb a
/// subprocess's scheduling under a fully parallel `cargo test --workspace`.
const RETIRE_BUDGET: Duration = Duration::from_secs(10);

/// Poll `condition` until it holds or [`RETIRE_BUDGET`] elapses.
fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    let deadline = std::time::Instant::now() + RETIRE_BUDGET;
    while std::time::Instant::now() < deadline {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    condition()
}

/// PIDs of `ironmem serve --listen` daemons bound to `sock`, found the same way
/// `scripts/install-ironmem.sh` finds them: `--listen` only (never a `--connect`
/// shim), and only on this socket. Used here to reap a daemon that was
/// auto-spawned *detached* by a proxy, whose `Child` this test never owns.
/// `sock` lives in a per-test tempdir, so this cannot match another test's
/// daemon even under a fully parallel run.
fn listening_daemon_pids(sock: &Path) -> Vec<u32> {
    let out = Command::new("pgrep")
        .args(["-f", "ironmem serve"])
        .output()
        .expect("pgrep must run");
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .filter_map(|pid| pid.parse::<u32>().ok())
        .filter(|pid| {
            let args = Command::new("ps")
                .args(["-o", "args=", "-p", &pid.to_string()])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
                .unwrap_or_default();
            args.contains("--listen") && args.contains(&sock.display().to_string())
        })
        .collect()
}

/// The install-upgrade contract, end to end through the real binary and a real
/// `SIGTERM`: `scripts/install-ironmem.sh` retires the shared daemon so the
/// next MCP client picks up the newly installed build, and that must NOT cost
/// anyone their live session.
///
/// A signalled daemon must therefore:
///   1. stop admitting new clients at once (refused, so a `--connect` proxy
///      auto-spawns a fresh daemon rather than hanging on a dead backlog),
///   2. keep serving the client that was already attached when the signal
///      landed, for as long as that client stays attached,
///   3. let a NEW `--connect` proxy auto-spawn a successor daemon (the point of
///      the whole exercise: that client is the one running the new binary),
///   4. exit once that last old client disconnects, without ever needing a
///      `SIGKILL` — and WITHOUT unlinking the successor's socket on the way
///      out, even though the successor now owns the same path (C1, arriving via
///      the drain window this retire path introduces).
///
/// The idle window is 600s so the idle timer cannot account for any of it: the
/// only thing that can end this daemon is the signal.
#[test]
fn sigterm_retires_the_daemon_without_cutting_off_an_attached_client() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let sock = temp.path().join("daemon.sock");
    std::fs::create_dir_all(&home).unwrap();

    let mut listen_cmd = Command::new(bin());
    listen_cmd
        .arg("serve")
        .arg("--listen")
        .arg(&sock)
        .env("IRONMEM_DAEMON_IDLE_SECS", "600");
    base_env(&mut listen_cmd, &home, &db_path);
    let mut daemon = KillOnDrop(
        listen_cmd
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("daemon must spawn"),
    );

    // An attached client, mid-session: connected, initialized, holding on.
    let stream = connect_with_retry(&sock);
    let mut writer = stream.try_clone().unwrap();
    let mut reader = BufReader::new(stream);
    writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{}}\n")
        .unwrap();
    writer.flush().unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    assert!(line.contains("\"protocolVersion\""));

    // What the installer does.
    let pid = daemon.0.id();
    let signalled = Command::new("kill")
        .arg("-TERM")
        .arg(pid.to_string())
        .status()
        .expect("kill must run");
    assert!(signalled.success(), "SIGTERM must be delivered to {pid}");

    // 1. New clients are refused promptly — the daemon really did receive and
    //    act on the signal, which is the wiring this test exists to prove.
    assert!(
        wait_for(|| UnixStream::connect(&sock).is_err()),
        "a SIGTERMed daemon must stop admitting new connections"
    );

    // 2. The attached client is untouched and still answered. This is the
    //    whole safety argument for signalling on install: an ungraceful
    //    shutdown closes this socket and the exchange below fails.
    let mut line2 = String::new();
    let served = writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"initialize\",\"params\":{}}\n")
        .and_then(|()| writer.flush())
        .and_then(|()| reader.read_line(&mut line2));
    assert!(
        matches!(served, Ok(n) if n > 0),
        "a client attached before the SIGTERM must keep being served, not cut \
         off mid-session (outcome: {served:?})"
    );
    assert!(
        line2.contains("\"protocolVersion\""),
        "and must get real responses, not a truncated stream: {line2}"
    );
    assert!(
        daemon.0.try_wait().unwrap().is_none(),
        "the daemon must still be draining, not already exited"
    );

    // 3. A NEW client auto-spawns a successor and is served. This is the upgrade
    //    actually taking effect: the refusal in step 1 is only useful because it
    //    routes new clients to a daemon running the freshly installed binary.
    let mut connect_cmd = Command::new(bin());
    connect_cmd
        .arg("serve")
        .arg("--connect")
        .arg(&sock)
        .env("IRONMEM_DAEMON_IDLE_SECS", "600");
    base_env(&mut connect_cmd, &home, &db_path);
    let (mut proxy_child, proxy_line) = run_initialize_over_stdio(connect_cmd);
    assert!(
        proxy_line.contains("\"protocolVersion\""),
        "a new client arriving after the retire must auto-spawn a successor \
         daemon and be served by it: {proxy_line}"
    );
    assert!(proxy_child.wait().unwrap().success());
    let successors = listening_daemon_pids(&sock);
    assert!(
        successors.iter().any(|&p| p != pid),
        "a successor daemon must now be listening on {} (found {successors:?}, \
         old daemon was {pid})",
        sock.display()
    );

    // The old daemon is still draining its own client throughout, undisturbed
    // by the successor taking over the socket.
    let mut line3 = String::new();
    let still_served = writer
        .write_all(b"{\"jsonrpc\":\"2.0\",\"id\":3,\"method\":\"initialize\",\"params\":{}}\n")
        .and_then(|()| writer.flush())
        .and_then(|()| reader.read_line(&mut line3));
    assert!(
        matches!(still_served, Ok(n) if n > 0),
        "the draining daemon must keep serving its client even after a \
         successor claims the socket (outcome: {still_served:?})"
    );

    // 4. Releasing the last client completes the drain and the old daemon exits
    //    on its own. No SIGKILL.
    drop(writer);
    drop(reader);
    assert!(
        wait_for(|| daemon.0.try_wait().unwrap().is_some()),
        "the daemon must exit once its last drained connection closes"
    );
    let status = daemon.0.try_wait().unwrap().unwrap();
    assert!(
        status.success(),
        "a signalled retire is a requested shutdown, not a failure: {status:?}"
    );

    // C1: and its cleanup must have left the LIVE successor's socket alone.
    // An unconditional `remove_file` at exit would unlink it here, stranding
    // every future client on a daemon they can no longer find.
    assert!(
        sock.exists(),
        "the exiting daemon must not remove the successor's socket"
    );
    assert!(
        UnixStream::connect(&sock).is_ok(),
        "and the successor must still be reachable there"
    );

    for successor in listening_daemon_pids(&sock) {
        let _ = Command::new("kill")
            .arg("-TERM")
            .arg(successor.to_string())
            .status();
    }
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
