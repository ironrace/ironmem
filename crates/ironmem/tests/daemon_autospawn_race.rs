//! Task 9 (#190) acceptance: many `serve --connect` proxies racing against a
//! socket with no daemon yet must single-flight the auto-spawn — exactly one
//! detached `ironmem serve --listen` daemon comes up, and every proxy ends up
//! talking to that ONE daemon/App/DB, not a daemon-per-client.
//!
//! Unlike the lock-mechanics unit tests in `crates/ironmem/src/mcp/daemon.rs`
//! (which use an injectable fake "spawn" to avoid launching a real process),
//! this test drives the REAL compiled `ironmem` binary via
//! `CARGO_BIN_EXE_ironmem` end-to-end, because the thing actually being
//! proven here — that concurrent OS processes racing `current_exe()` +
//! `Command::spawn` converge on one daemon — cannot be faked without spawning
//! real processes.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use ironmem::db::schema::Database;
use serde_json::{json, Value};

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

fn base_command(home: &Path, db_path: &Path, sock: &Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.arg("serve")
        .arg("--connect")
        .arg(sock)
        .env("HOME", home)
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_MCP_MODE", "trusted")
        .env("IRONMEM_AUTO_BOOTSTRAP", "0")
        // Idle window: the auto-spawned daemon should clean itself up after
        // this test's proxies all disconnect, rather than lingering as an
        // orphaned background process for the default 300s. NOT razor-thin:
        // the daemon's idle timer arms the instant it starts (by design —
        // see mcp::daemon::serve_accept_loop — a `--listen` daemon nobody
        // ever connects to must still clean itself up), so under a heavily
        // parallel `cargo test --workspace` run, 5 real subprocesses racing
        // to fork/exec/connect can occasionally take longer than a very
        // short window to land their first connection. 10s gives that real
        // OS-scheduling jitter comfortable headroom while still keeping the
        // test itself fast (it does not wait out the full window on the
        // happy path — only the trailing self-cleanup check does).
        .env("IRONMEM_DAEMON_IDLE_SECS", "10")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    cmd
}

/// Count processes whose command line names this exact daemon socket in a
/// `serve --listen <sock>` invocation, via `ps` (portable enough for macOS +
/// Linux CI; `-ww` disables argv truncation).
fn count_daemon_processes(sock: &Path) -> usize {
    let needle = format!("--listen {}", sock.display());
    let out = Command::new("ps")
        .args(["-A", "-ww", "-o", "command"])
        .output()
        .expect("ps must run");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines().filter(|line| line.contains(&needle)).count()
}

#[test]
fn concurrent_proxies_single_flight_one_daemon_and_share_one_db() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("shared.sqlite3");
    let sock = temp.path().join("daemon.sock");
    std::fs::create_dir_all(&home).unwrap();

    const N: usize = 5;

    // Spawn all N proxies back-to-back, BEFORE reading from any of them, so
    // their initial connect attempts genuinely race against the (as yet
    // nonexistent) socket concurrently rather than sequentially.
    let mut children: Vec<_> = (0..N)
        .map(|_| {
            base_command(&home, &db_path, &sock)
                .spawn()
                .expect("proxy must spawn")
        })
        .collect();

    // Now drive each proxy's handshake in turn: write initialize + a uniquely
    // identifiable add_drawer call, read both responses back, THEN close
    // stdin. Reading before closing stdin avoids racing "client closed its
    // input" against "the daemon's response is still in flight" — the same
    // hazard covered by the daemon.rs proxy unit tests.
    for (i, child) in children.iter_mut().enumerate() {
        let mut stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut reader = BufReader::new(stdout);

        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
        )
        .unwrap();
        writeln!(
            stdin,
            "{}",
            json!({
                "jsonrpc":"2.0","id":2,"method":"tools/call",
                "params":{
                    "name":"add_drawer",
                    "arguments":{
                        "wing":"race",
                        "room":format!("client-{i}"),
                        "content":format!("hello from client {i}")
                    }
                }
            })
        )
        .unwrap();

        let mut init_line = String::new();
        reader
            .read_line(&mut init_line)
            .unwrap_or_else(|e| panic!("client {i} initialize read failed: {e}"));
        assert!(
            init_line.contains("\"protocolVersion\""),
            "client {i} initialize response: {init_line}"
        );

        let mut add_line = String::new();
        reader
            .read_line(&mut add_line)
            .unwrap_or_else(|e| panic!("client {i} add_drawer read failed: {e}"));

        // Parse as JSON-RPC and require an actual completed write acknowledgement,
        // not merely a success-shaped envelope. Before this fix a
        // `warming_up` no-op body (then returned by `handle_add_drawer`; see
        // handle_add_drawer in src/mcp/tools/drawers.rs) would satisfy a bare
        // substring check like `contains("\"id\":2")` while performing no write
        // at all — that's precisely the silent-data-loss hole this assertion
        // must close.
        let envelope: Value = serde_json::from_str(&add_line).unwrap_or_else(|e| {
            panic!("client {i} add_drawer response is not JSON: {e}\nraw: {add_line}")
        });
        assert_eq!(
            envelope["id"], 2,
            "client {i} add_drawer response has wrong JSON-RPC id: {add_line}"
        );
        let result = envelope
            .get("result")
            .unwrap_or_else(|| panic!("client {i} add_drawer response has no result: {add_line}"));
        assert!(
            !result
                .get("isError")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            "client {i} add_drawer response reported isError: {add_line}"
        );
        let text = result
            .get("content")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("text"))
            .and_then(Value::as_str)
            .unwrap_or_else(|| {
                panic!("client {i} add_drawer response missing content[0].text: {add_line}")
            });
        let payload: Value = serde_json::from_str(text).unwrap_or_else(|e| {
            panic!("client {i} add_drawer content text is not JSON: {e}\ntext: {text}")
        });
        assert!(
            payload.get("warming_up").is_none(),
            "client {i} add_drawer returned a warming_up no-op instead of a real write: {text}"
        );
        assert_eq!(
            payload["success"].as_bool(),
            Some(true),
            "client {i} add_drawer payload missing success:true: {text}"
        );
        assert!(
            payload
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .is_some(),
            "client {i} add_drawer payload missing a persisted drawer id: {text}"
        );

        drop(stdin);
    }

    for (i, child) in children.iter_mut().enumerate() {
        let status = child.wait().unwrap();
        assert!(status.success(), "proxy {i} must exit cleanly: {status:?}");
    }

    // Exactly one daemon process was spawned for this socket, no matter how
    // many proxies raced to start it.
    assert_eq!(
        count_daemon_processes(&sock),
        1,
        "exactly one daemon must have been auto-spawned for {}",
        sock.display()
    );

    // All N clients' writes landed in the SAME database through the SAME
    // App/daemon — not N independent per-proxy databases.
    let db = Database::open(&db_path).expect("shared db must be readable");
    let rows = db
        .get_drawers(Some("race"), None, 100)
        .expect("query must succeed");
    assert_eq!(rows.len(), N, "every client's add_drawer must be visible");
    for i in 0..N {
        assert!(
            rows.iter()
                .any(|d| d.content.contains(&format!("hello from client {i}"))),
            "client {i}'s content must be present among the shared rows"
        );
    }

    // Hygiene: the auto-spawned daemon's idle timer (10s, set above) should
    // clean it up shortly after every proxy above disconnected. Give it a
    // generous bound (comfortably longer than the idle window itself, plus
    // scheduling slack under a busy `cargo test --workspace` run) and confirm
    // no orphaned daemon process or socket file is left behind.
    for _ in 0..150 {
        if count_daemon_processes(&sock) == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    assert_eq!(
        count_daemon_processes(&sock),
        0,
        "the auto-spawned daemon must self-terminate after its idle window"
    );
}
