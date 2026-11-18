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
fn missing_database_is_warn_and_db_flag_is_honored() {
    // No IRONMEM_DB_PATH; the DB path comes from the `--db` flag and points at a
    // nonexistent file (the fresh-install state). It must be a non-blocking
    // warning, exit 0.
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let missing_db = temp.path().join("does-not-exist.sqlite3");
    std::fs::create_dir_all(&home).unwrap();

    let out = Command::new(bin())
        .env("HOME", &home)
        .env("CODEX_HOME", home.join(".codex"))
        .env("IRONMEM_EMBED_MODE", "noop")
        .arg("doctor")
        .arg("--db")
        .arg(&missing_db)
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "a missing database is a warning, not a blocking failure: {out:?}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("[WARN]"));
    assert!(stdout.contains("database not found"));
    assert!(stdout.contains("no blocking failures"));
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

/// Write `~/.claude/settings.json` under `home`.
fn write_claude_settings(home: &Path, doc: serde_json::Value) {
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    std::fs::write(home.join(".claude").join("settings.json"), doc.to_string()).unwrap();
}

/// A settings document with one hook group per `(event, command)`.
fn hooks_doc(commands: &[(&str, &str)]) -> serde_json::Value {
    let mut hooks = serde_json::Map::new();
    for (event, cmd) in commands {
        hooks.insert(
            (*event).to_string(),
            serde_json::json!([{ "hooks": [{ "type": "command", "command": cmd }] }]),
        );
    }
    serde_json::json!({ "hooks": hooks })
}

const BARE_HOOKS: [(&str, &str); 4] = [
    (
        "SessionStart",
        "/x/ironmem hook session-start --harness claude-code",
    ),
    (
        "UserPromptSubmit",
        "/x/ironmem hook user-prompt-submit --harness claude-code",
    ),
    ("Stop", "/x/ironmem hook stop --harness claude-code"),
    (
        "PreCompact",
        "/x/ironmem hook precompact --harness claude-code",
    ),
];

/// Insert one mined drawer filed at `filed_at` (UTC) into an initialized store.
/// `'mine'` copies `crate::db::drawers::MINED_ADDED_BY`, which is `pub(crate)`.
fn insert_mined_row(db_path: &Path, filed_at: &str) {
    let conn = rusqlite::Connection::open(db_path).unwrap();
    conn.execute(
        "INSERT INTO drawers (id, content, embedding, wing, room, source_file, added_by, filed_at)
         VALUES (?1, 'mined', x'', 'w', 'general', 'f.jsonl', 'mine', ?2)",
        rusqlite::params!["d".repeat(32), filed_at],
    )
    .unwrap();
}

#[test]
fn non_plugin_read_only_hooks_say_plainly_that_nothing_is_mined() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    init_db(&home, &db_path);
    insert_mined_row(&db_path, "2026-01-01 00:00:00");
    write_claude_settings(&home, hooks_doc(&BARE_HOOKS));

    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env_remove("IRONMEM_MCP_MODE")
        .output()
        .unwrap();

    assert!(
        out.status.success(),
        "warnings must not fail doctor: {out:?}"
    );
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("Claude Code hooks: all 4 wired"),
        "{stdout}"
    );
    assert!(
        stdout.contains("Stop: read-only (IRONMEM_MCP_MODE unset, so the default) -> not mined"),
        "{stdout}"
    );
    assert!(stdout.contains("no wired hook mines"), "{stdout}");
    assert!(
        stdout.contains("(2026-01-01 00:00:00 UTC): transcripts are not being mined"),
        "{stdout}"
    );
}

#[test]
fn missing_prompt_hook_is_named() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    init_db(&home, &db_path);
    write_claude_settings(
        &home,
        hooks_doc(&[BARE_HOOKS[0], BARE_HOOKS[2], BARE_HOOKS[3]]),
    );

    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env_remove("IRONMEM_MCP_MODE")
        .output()
        .unwrap();

    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(stdout.contains("missing UserPromptSubmit"), "{stdout}");
    assert!(stdout.contains("prompt recall is off"), "{stdout}");
    assert!(stdout.contains("no mined rows yet"), "{stdout}");
}

#[test]
fn plugin_install_and_trusted_hooks_report_mining_on() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    std::fs::create_dir_all(&home).unwrap();
    init_db(&home, &db_path);

    write_claude_settings(
        &home,
        serde_json::json!({ "enabledPlugins": { "ironmem@ironrace": true } }),
    );
    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env_remove("IRONMEM_MCP_MODE")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains("supplied by the enabled ironmem plugin"),
        "{stdout}"
    );
    assert!(stdout.contains("Stop and PreCompact are mined"), "{stdout}");
    assert!(
        stdout.contains("Stop: trusted (the plugin wrapper's default) -> mined"),
        "{stdout}"
    );

    let trusted: Vec<(&str, String)> = BARE_HOOKS
        .iter()
        .map(|(e, c)| (*e, format!("IRONMEM_MCP_MODE=trusted {c}")))
        .collect();
    let refs: Vec<(&str, &str)> = trusted.iter().map(|(e, c)| (*e, c.as_str())).collect();
    write_claude_settings(&home, hooks_doc(&refs));
    let out = doctor_command(&home, &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env_remove("IRONMEM_MCP_MODE")
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let stdout = String::from_utf8(out.stdout).unwrap();
    assert!(
        stdout.contains(
            "Stop and PreCompact are mined — Stop: trusted (set in the hook command) -> mined; \
             PreCompact: trusted (set in the hook command) -> mined"
        ),
        "{stdout}"
    );
}
