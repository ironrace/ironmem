//! Integration tests for `ironmem doctor` (issue #142).
//!
//! Each test drives the real binary against temp dirs so the CLI contract —
//! exit codes, text output, and `--json` shape — is exercised end to end.

use std::path::Path;
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

/// A `doctor` invocation with a hermetic environment: temp HOME, temp
/// CODEX_HOME (absent on disk), and an explicit DB path. Embed mode and MCP
/// mode default to the production defaults unless the caller overrides them.
fn doctor_command(home: &Path, db_path: &Path) -> Command {
    let mut cmd = Command::new(bin());
    cmd.env("HOME", home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("IRONMEM_DB_PATH", db_path)
        .arg("doctor");
    cmd
}

/// Materialize a current-schema database at `db_path` by running `report`,
/// which opens and migrates the store.
fn init_db(home: &Path, db_path: &Path) {
    let out = Command::new(bin())
        .env("HOME", home)
        .env("IRONMEM_DB_PATH", db_path)
        .arg("report")
        .output()
        .unwrap();
    assert!(out.status.success(), "report (db init) failed: {out:?}");
}

#[test]
fn healthy_setup_exits_zero() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    init_db(&home, &db_path);

    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "doctor should exit 0 when healthy: {out:?}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("ironmem doctor"));
    assert!(stdout.contains("schema v"));
    assert!(stdout.contains("no blocking failures"));
}

#[test]
fn missing_model_in_real_mode_exits_nonzero() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let empty_model_dir = temp.path().join("no-model");
    std::fs::create_dir_all(&home).unwrap();
    init_db(&home, &db_path);

    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "real")
        .env("IRONMEM_MODEL_DIR", &empty_model_dir)
        .output()
        .unwrap();

    assert!(
        !out.status.success(),
        "doctor must exit non-zero when the model is missing in real mode: {out:?}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[FAIL]"));
    assert!(stdout.to_lowercase().contains("model"));
    assert!(stdout.contains("blocking setup failures found"));
}

#[test]
fn reports_read_only_and_trusted_mcp_mode() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    init_db(&home, &db_path);

    let read_only = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_MCP_MODE", "read-only")
        .output()
        .unwrap();
    assert!(read_only.status.success());
    assert!(String::from_utf8(read_only.stdout)
        .unwrap()
        .contains("read-only"));

    let trusted = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_MCP_MODE", "trusted")
        .output()
        .unwrap();
    assert!(trusted.status.success());
    assert!(String::from_utf8(trusted.stdout)
        .unwrap()
        .contains("trusted"));
}

#[test]
fn invalid_harness_config_warns_but_does_not_block() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    init_db(&home, &db_path);
    // Present but malformed Claude config → Malformed state → Warn (advisory).
    std::fs::write(home.join(".claude.json"), "{ not valid json").unwrap();

    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "a misconfigured harness is a warning, not a blocking failure: {out:?}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[WARN]"));
    assert!(stdout.contains("Claude Code"));
}

#[test]
fn registered_harnesses_report_ok_end_to_end() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let codex_home = home.join(".codex");
    std::fs::create_dir_all(&codex_home).unwrap();
    init_db(&home, &db_path);
    // Claude registers ironmem in ~/.claude.json.
    std::fs::write(
        home.join(".claude.json"),
        r#"{"mcpServers":{"ironmem":{"command":"ironmem","args":["serve"]}}}"#,
    )
    .unwrap();
    // Codex registers ironmem via $CODEX_HOME/config.toml (exercises the
    // CODEX_HOME resolution path).
    std::fs::write(
        codex_home.join("config.toml"),
        "[mcp_servers.ironmem]\ncommand = \"ironmem\"\nargs = [\"serve\"]\n",
    )
    .unwrap();

    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "registered harnesses are healthy: {out:?}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("Claude Code: ironmem MCP server registered"));
    assert!(stdout.contains("Codex: ironmem MCP server registered"));
}

#[test]
fn json_output_is_machine_readable() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    init_db(&home, &db_path);

    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .arg("--json")
        .output()
        .unwrap();

    assert!(out.status.success());
    let stdout = String::from_utf8(out.stdout).unwrap();
    let value: serde_json::Value =
        serde_json::from_str(&stdout).expect("--json must emit valid JSON");
    let checks = value
        .get("checks")
        .and_then(|c| c.as_array())
        .expect("report has a checks array");
    assert!(!checks.is_empty());
    for check in checks {
        assert!(check.get("name").and_then(|v| v.as_str()).is_some());
        assert!(check.get("status").and_then(|v| v.as_str()).is_some());
        assert!(check.get("summary").and_then(|v| v.as_str()).is_some());
    }
    // The model check is `info` under noop mode and serializes lowercase.
    assert!(stdout.contains("\"status\": \"info\"") || stdout.contains("\"status\":\"info\""));
}
