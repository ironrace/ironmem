//! Task 10 (#190) acceptance: on non-Unix platforms, `--listen`/`--connect`
//! have no socket transport (Unix-domain sockets only) and must degrade
//! gracefully to the exact in-process stdio `serve` behavior, rather than
//! erroring or hanging.
//!
//! Entirely `cfg(not(unix))`: on Unix this file compiles to zero tests (the
//! real transport is covered by `crates/ironmem/tests/daemon_autospawn_race.rs`
//! and the `mcp::daemon` unit tests instead). This is the test that runs when
//! CI exercises a non-Unix target.
#![cfg(not(unix))]

use serde_json::json;
use std::io::Write;
use std::process::Stdio;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

fn base_command(home: &std::path::Path, db_path: &std::path::Path) -> std::process::Command {
    let mut cmd = std::process::Command::new(bin());
    cmd.env("HOME", home)
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_AUTO_BOOTSTRAP", "0")
        .env("IRONMEM_MCP_MODE", "trusted");
    cmd
}

/// `serve --listen <path>` on a non-Unix platform: the flag is consumed and
/// ignored (Task 10's `#[cfg(not(unix))] let _ = listen;` arm in `main.rs`),
/// falling straight through to the in-process stdio server — identical
/// behavior to bare `serve`.
#[test]
fn serve_listen_falls_back_to_in_process_stdio_on_non_unix() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();

    let mut serve = base_command(&home, &db_path)
        .arg("serve")
        .arg("--listen")
        .arg(temp.path().join("would-be-a-socket-on-unix"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = serve.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
        )
        .unwrap();
    }
    let output = serve.wait_with_output().unwrap();
    assert!(output.status.success(), "serve failed: {:?}", output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("\"protocolVersion\":\"2024-11-05\""),
        "non-unix --listen must still answer initialize via in-process serve: {stdout}"
    );
}

/// `serve --connect <path>` on a non-Unix platform: same fallback contract as
/// `--listen` above (Task 10's `#[cfg(not(unix))] let _ = (connect,
/// no_autospawn);` arm).
#[test]
fn serve_connect_falls_back_to_in_process_stdio_on_non_unix() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();

    let mut serve = base_command(&home, &db_path)
        .arg("serve")
        .arg("--connect")
        .arg(temp.path().join("would-be-a-socket-on-unix"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    {
        let stdin = serve.stdin.as_mut().unwrap();
        writeln!(
            stdin,
            "{}",
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}})
        )
        .unwrap();
    }
    let output = serve.wait_with_output().unwrap();
    assert!(output.status.success(), "serve failed: {:?}", output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(
        stdout.contains("\"protocolVersion\":\"2024-11-05\""),
        "non-unix --connect must still answer initialize via in-process serve: {stdout}"
    );
}
