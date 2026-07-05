//! Live-server integration test for the read-only dashboard.
//!
//! Starts the real `ironmem dashboard` binary on an ephemeral port against a
//! migrated temp DB, discovers the bound address from the `--json` startup
//! line, issues real HTTP requests over TCP, and asserts the DB is unchanged
//! (schema_version + drawer count) after the request sweep.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use ironmem::db::schema::Database;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

/// Seed a migrated DB with a single drawer; return the drawer count.
fn seed_db(db_path: &Path) -> usize {
    let db = Database::open(db_path).unwrap();
    db.migrate().unwrap();
    let emb = vec![0.0f32; ironrace_embed::embedder::EMBED_DIM];
    let id = ironmem::db::drawers::generate_id("live content", "wing-x", "room-x");
    db.insert_drawer(
        &id,
        "live content",
        &emb,
        "wing-x",
        "room-x",
        "src/x.rs",
        "test",
    )
    .unwrap();
    drawer_count(db_path)
}

fn drawer_count(db_path: &Path) -> usize {
    Database::open_read_only(db_path)
        .unwrap()
        .count_drawers(None)
        .unwrap()
}

struct DashboardProcess {
    child: Child,
    addr: String,
}

impl DashboardProcess {
    fn kill_and_wait(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    fn wait_for_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = self.child.try_wait().expect("poll dashboard child") {
                return Some(status);
            }
            if Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

impl Drop for DashboardProcess {
    fn drop(&mut self) {
        self.kill_and_wait();
    }
}

/// Spawn the dashboard with `--port 0 --json` and read the bound address from
/// the JSON startup line on stdout.
fn spawn_dashboard(db_path: &Path) -> DashboardProcess {
    let child = Command::new(bin())
        .arg("dashboard")
        .arg("--db")
        .arg(db_path)
        .arg("--port")
        .arg("0")
        .arg("--json")
        .arg("--exit-on-stdin-close")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn dashboard");

    let mut dashboard = DashboardProcess {
        child,
        addr: String::new(),
    };

    let stdout = dashboard.child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader.read_line(&mut line).expect("read startup line");

    let meta: serde_json::Value = serde_json::from_str(line.trim())
        .unwrap_or_else(|e| panic!("bad startup json ({e}): {line}"));
    let url = meta["url"]
        .as_str()
        .expect("url in startup json")
        .to_string();
    // url is like http://127.0.0.1:54321 — strip scheme to get host:port.
    dashboard.addr = url.strip_prefix("http://").expect("http url").to_string();
    // Drain stderr in the background so the child never blocks on a full pipe,
    // and so we can surface server-side errors if a request misbehaves.
    if let Some(err) = dashboard.child.stderr.take() {
        std::thread::spawn(move || {
            let mut r = BufReader::new(err);
            let mut l = String::new();
            while r.read_line(&mut l).unwrap_or(0) > 0 {
                eprint!("[server] {l}");
                l.clear();
            }
        });
    }
    dashboard
}

/// Issue a single raw HTTP/1.1 request and return (status_line, body).
fn http_request(addr: &str, method: &str, path: &str) -> (String, String) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut stream = loop {
        match TcpStream::connect(addr) {
            Ok(s) => break s,
            Err(_) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => panic!("connect {addr}: {e}"),
        }
    };
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();

    // Read until EOF. A peer reset after the full response has been delivered is
    // treated as end-of-stream rather than a hard failure.
    let mut bytes: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => bytes.extend_from_slice(&buf[..n]),
            Err(e)
                if e.kind() == std::io::ErrorKind::ConnectionReset
                    || e.kind() == std::io::ErrorKind::UnexpectedEof =>
            {
                break;
            }
            Err(e) => panic!("read response: {e}"),
        }
    }
    let raw = String::from_utf8_lossy(&bytes).into_owned();
    let mut parts = raw.splitn(2, "\r\n\r\n");
    let head = parts.next().unwrap_or("");
    let body = parts.next().unwrap_or("").to_string();
    let status_line = head.lines().next().unwrap_or("").to_string();
    (status_line, body)
}

#[test]
fn live_dashboard_serves_readonly_over_tcp() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("memory.sqlite3");

    let count_before = seed_db(&db_path);
    let version_before = Database::open_read_only(&db_path)
        .unwrap()
        .schema_version()
        .unwrap();

    let mut dashboard = spawn_dashboard(&db_path);

    // GET /api/summary → 200 with the seeded drawer count.
    let (get_status, get_body) = http_request(&dashboard.addr, "GET", "/api/summary");
    assert!(get_status.contains("200"), "summary status: {get_status}");
    let summary: serde_json::Value = serde_json::from_str(&get_body)
        .unwrap_or_else(|e| panic!("summary json ({e}): {get_body}"));
    assert_eq!(summary["total_drawers"].as_u64(), Some(count_before as u64));

    // POST / → 405 (only GET/HEAD are served).
    let (post_status, _post_body) = http_request(&dashboard.addr, "POST", "/");
    assert!(post_status.contains("405"), "POST status: {post_status}");

    // Tear down the server.
    dashboard.kill_and_wait();

    // The DB must be byte-for-byte unchanged in schema + row count.
    let version_after = Database::open_read_only(&db_path)
        .unwrap()
        .schema_version()
        .unwrap();
    assert_eq!(version_before, version_after, "schema_version changed");
    assert_eq!(count_before, drawer_count(&db_path), "drawer count changed");
}

#[test]
fn live_dashboard_exits_when_parent_stdin_closes() {
    let temp = tempfile::tempdir().unwrap();
    let db_path = temp.path().join("memory.sqlite3");
    seed_db(&db_path);

    let mut dashboard = spawn_dashboard(&db_path);
    drop(dashboard.child.stdin.take());

    let status = dashboard
        .wait_for_exit(Duration::from_secs(5))
        .expect("dashboard should exit after stdin closes");
    assert!(status.success(), "dashboard exited with {status:?}");
}
