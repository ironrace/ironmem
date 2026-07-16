//! Task 15 (#190) acceptance: bind one real `serve --listen` daemon and open
//! N concurrent connections with MIXED harness `clientInfo`, each doing
//! `initialize` + an `add_drawer` WRITE in a burst. All N must succeed
//! against the single shared App/DB, dispatch must serialize cleanly (no
//! corruption), and each connection's attribution must land on the correct
//! harness — extending the existing in-process duplex-pipe test pattern
//! (`mcp::server`'s tests) to a real `UnixListener` behind a real,
//! separately-compiled `ironmem` process.
#![cfg(unix)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use ironmem::db::metrics::TokenUsageQuery;
use ironmem::db::schema::Database;
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

/// Kill a spawned child on drop, so a failing assertion (which unwinds past
/// the manual `kill`/`wait` at the end of the test) still cleans up rather
/// than leaking a daemon process.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn concurrent_mixed_harness_burst_all_succeed_on_one_shared_db() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("shared.sqlite3");
    let sock = temp.path().join("daemon.sock");
    std::fs::create_dir_all(&home).unwrap();

    let daemon = Command::new(bin())
        .arg("serve")
        .arg("--listen")
        .arg(&sock)
        .env("HOME", &home)
        .env("IRONMEM_DB_PATH", &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_MCP_MODE", "trusted")
        .env("IRONMEM_AUTO_BOOTSTRAP", "0")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("daemon must spawn");
    let mut daemon = KillOnDrop(daemon);

    // Mixed harness clientInfo names, exercising real classify_client_info
    // substring matching plus the "unrecognized harness" fallback path.
    const HARNESSES: &[(&str, &str)] = &[
        ("claude-code", "claude"),
        ("codex-cli", "codex"),
        ("gemini", "gemini"),
        ("some-other-tool", "claude"), // unrecognized -> falls back to claude
    ];
    const CLIENTS_PER_HARNESS: usize = 3;

    connect_with_retry(&sock); // block until the daemon has actually bound

    let mut handles = Vec::new();
    for (i, (client_name, expected_harness)) in harnesses_cycle(HARNESSES, CLIENTS_PER_HARNESS) {
        let sock = sock.clone();
        handles.push(std::thread::spawn(move || {
            let session_id = format!("burst-session-{i}");
            let stream = connect_with_retry(&sock);
            let mut writer = stream.try_clone().unwrap();
            let mut reader = BufReader::new(stream);

            let init_req = json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "sessionId": session_id,
                    "clientInfo": {"name": client_name, "version": "1.0.0"}
                }
            });
            writeln!(writer, "{init_req}").unwrap();
            let mut init_line = String::new();
            reader.read_line(&mut init_line).unwrap();
            assert!(
                init_line.contains("\"protocolVersion\""),
                "client {i} ({client_name}) initialize failed: {init_line}"
            );

            let room = format!("burst-client-{i}");
            let add_req = json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": {
                    "name": "add_drawer",
                    "arguments": {
                        "wing": "burst",
                        "room": room,
                        "content": format!("hello from client {i} ({client_name})")
                    }
                }
            });
            writeln!(writer, "{add_req}").unwrap();
            let mut add_line = String::new();
            reader.read_line(&mut add_line).unwrap();
            assert!(
                add_line.contains("\"id\":2") && !add_line.contains("\"isError\":true"),
                "client {i} ({client_name}) add_drawer failed: {add_line}"
            );

            (i, session_id, expected_harness.to_string())
        }));
    }

    let mut expected_harness_by_session: HashMap<String, String> = HashMap::new();
    for handle in handles {
        let (_i, session_id, expected_harness) =
            handle.join().expect("client thread must not panic");
        expected_harness_by_session.insert(session_id, expected_harness);
    }
    let total_clients = HARNESSES.len() * CLIENTS_PER_HARNESS;
    assert_eq!(expected_harness_by_session.len(), total_clients);

    // Clean shutdown before inspecting the DB file.
    daemon.0.kill().ok();
    daemon.0.wait().ok();

    // Single shared App/DB: every client's add_drawer write landed, none
    // corrupted or lost, despite the concurrent burst.
    let db = Database::open(&db_path).expect("shared db must be readable");
    let rows = db
        .get_drawers(Some("burst"), None, 100)
        .expect("query must succeed");
    assert_eq!(
        rows.len(),
        total_clients,
        "every concurrent client's add_drawer must be visible exactly once"
    );

    // Per-connection attribution: each session's recorded harness must match
    // what ITS OWN clientInfo should have classified to — proving dispatch
    // serialized cleanly rather than cross-attributing responses between
    // concurrently-open connections.
    let usage_rows = db
        .query_token_usage(&TokenUsageQuery::default())
        .expect("query must succeed");
    for (session_id, expected_harness) in &expected_harness_by_session {
        let matching: Vec<_> = usage_rows
            .iter()
            .filter(|r| r.session_id.as_deref() == Some(session_id.as_str()))
            .collect();
        assert!(
            !matching.is_empty(),
            "session {session_id} must have recorded mcp_response rows"
        );
        assert!(
            matching.iter().all(|r| &r.harness == expected_harness),
            "session {session_id} must be attributed to harness {expected_harness}, got: {:?}",
            matching.iter().map(|r| &r.harness).collect::<Vec<_>>()
        );
    }
}

/// Cycle `harnesses` to produce `count_per_harness` clients per harness,
/// paired with a stable index, in a form usable by a plain `for` loop.
fn harnesses_cycle(
    harnesses: &'static [(&'static str, &'static str)],
    count_per_harness: usize,
) -> Vec<(usize, (&'static str, &'static str))> {
    harnesses
        .iter()
        .cloned()
        .cycle()
        .take(harnesses.len() * count_per_harness)
        .enumerate()
        .collect()
}
