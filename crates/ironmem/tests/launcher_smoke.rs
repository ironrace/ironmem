//! End-to-end launcher smoke tests. A stub `claude`/`codex` executable on PATH
//! records the argv and cwd it was invoked with, so we verify the real binary
//! wiring without depending on a real assistant. Unix-only (uses a shell stub).

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

/// Create a stub executable named `name` in `dir` that writes its cwd to
/// `<record>.cwd` and each argv element (one per line) to `<record>.args`,
/// then exits with `exit_code`.
fn write_stub(dir: &Path, name: &str, record: &Path, exit_code: i32) -> PathBuf {
    let stub = dir.join(name);
    let script = format!(
        "#!/bin/sh\npwd > \"{rec}.cwd\"\n: > \"{rec}.args\"\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> \"{rec}.args\"; done\nexit {exit_code}\n",
        rec = record.display(),
        exit_code = exit_code
    );
    std::fs::write(&stub, script).unwrap();
    let mut perms = std::fs::metadata(&stub).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&stub, perms).unwrap();
    stub
}

/// Build an `ironmem` command with a hermetic environment (mirrors
/// crates/ironmem/tests/cli_smoke.rs) plus a PATH that contains `bin_dir`.
fn launcher_command(home: &Path, db_path: &Path, bin_dir: &Path) -> Command {
    let path_var = std::env::join_paths(std::iter::once(bin_dir.to_path_buf()).chain(
        std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
    ))
    .unwrap();
    let mut cmd = Command::new(bin());
    cmd.env("HOME", home)
        .env("IRONMEM_DB_PATH", db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_AUTO_BOOTSTRAP", "0")
        .env("IRONMEM_MCP_MODE", "trusted")
        .env("PATH", path_var);
    cmd
}

#[test]
fn claude_launcher_runs_stub_with_prompt_and_repo_cwd() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let bin_dir = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let record = temp.path().join("rec");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo\ncontent to mine").unwrap();
    write_stub(&bin_dir, "claude", &record, 0);

    let out = launcher_command(&home, &db_path, &bin_dir)
        .arg("claude")
        .arg(&repo)
        .arg("fix the login bug")
        .arg("--no-mcp-setup")
        .output()
        .unwrap();
    assert!(out.status.success(), "launcher failed: {out:?}");

    let recorded_args = std::fs::read_to_string(format!("{}.args", record.display())).unwrap();
    assert_eq!(recorded_args.trim(), "fix the login bug");

    let recorded_cwd = std::fs::read_to_string(format!("{}.cwd", record.display())).unwrap();
    let recorded_cwd = std::fs::canonicalize(recorded_cwd.trim()).unwrap();
    assert_eq!(recorded_cwd, std::fs::canonicalize(&repo).unwrap());
}

#[test]
fn claude_launcher_registers_mcp_server_by_default() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let bin_dir = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let record = temp.path().join("rec");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo\ncontent to mine").unwrap();
    write_stub(&bin_dir, "claude", &record, 0);

    // No --no-mcp-setup: the launcher must register the MCP server itself.
    let out = launcher_command(&home, &db_path, &bin_dir)
        .arg("claude")
        .arg(&repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "launcher failed: {out:?}");

    // The stub still ran (launch happened after registration).
    assert!(std::fs::metadata(format!("{}.args", record.display())).is_ok());

    // ~/.claude.json under the hermetic HOME now contains the ironmem server,
    // pointing `command` at this ironmem binary with `serve`.
    let claude_cfg = std::fs::read_to_string(home.join(".claude.json"))
        .expect("launcher should create ~/.claude.json");
    let v: serde_json::Value = serde_json::from_str(&claude_cfg).unwrap();
    let server = &v["mcpServers"]["ironmem"];
    assert!(
        server.is_object(),
        "ironmem MCP server should be registered: {claude_cfg}"
    );
    assert_eq!(server["args"][0].as_str(), Some("serve"));
    assert!(
        server["command"]
            .as_str()
            .unwrap_or_default()
            .contains("ironmem"),
        "command should point at the ironmem binary: {claude_cfg}"
    );
}

#[test]
fn launcher_errors_clearly_when_binary_missing() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let bin_dir = temp.path().join("empty-bin"); // contains no `codex`
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();

    // Use a PATH that is ONLY the empty bin dir so the system `codex` (if any)
    // is not found.
    let mut cmd = Command::new(bin());
    cmd.env("HOME", &home)
        .env("IRONMEM_DB_PATH", &db_path)
        .env("IRONMEM_EMBED_MODE", "noop")
        .env("IRONMEM_AUTO_BOOTSTRAP", "0")
        .env("IRONMEM_MCP_MODE", "trusted")
        .env("PATH", &bin_dir)
        .arg("codex")
        .arg(&repo)
        .arg("--no-mcp-setup");
    let out = cmd.output().unwrap();

    assert!(!out.status.success(), "should fail when binary missing");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("codex"),
        "stderr should name the binary: {stderr}"
    );
    assert!(
        stderr.contains("PATH"),
        "stderr should mention PATH: {stderr}"
    );
}

/// Like `launcher_command` but also sets `CODEX_HOME` for Codex config isolation.
fn launcher_command_codex(
    home: &Path,
    db_path: &Path,
    bin_dir: &Path,
    codex_home: &Path,
) -> Command {
    let mut cmd = launcher_command(home, db_path, bin_dir);
    cmd.env("CODEX_HOME", codex_home);
    cmd
}

#[test]
fn launcher_propagates_child_nonzero_exit_code() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let bin_dir = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let record = temp.path().join("rec");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo").unwrap();
    write_stub(&bin_dir, "claude", &record, 7);

    let out = launcher_command(&home, &db_path, &bin_dir)
        .arg("claude")
        .arg(&repo)
        .arg("--no-mcp-setup")
        .output()
        .unwrap();
    assert_eq!(
        out.status.code(),
        Some(7),
        "child exit code must propagate; stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn no_mcp_setup_skips_registration() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let bin_dir = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let record = temp.path().join("rec");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo").unwrap();
    write_stub(&bin_dir, "claude", &record, 0);

    let out = launcher_command(&home, &db_path, &bin_dir)
        .arg("claude")
        .arg(&repo)
        .arg("--no-mcp-setup")
        .output()
        .unwrap();
    assert!(out.status.success(), "launcher failed: {out:?}");
    // The stub still ran...
    assert!(std::fs::metadata(format!("{}.cwd", record.display())).is_ok());
    // ...but no Claude config was written.
    assert!(
        !home.join(".claude.json").exists(),
        "registration must be skipped"
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("skipping MCP setup"),
        "should announce skip: {stderr}"
    );
}

#[test]
fn codex_launcher_registers_mcp_server_by_default() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let bin_dir = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let record = temp.path().join("rec");
    let codex_home = temp.path().join("codex-home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo\ncontent to mine").unwrap();
    write_stub(&bin_dir, "codex", &record, 0);

    let out = launcher_command_codex(&home, &db_path, &bin_dir, &codex_home)
        .arg("codex")
        .arg(&repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "codex launcher failed: {out:?}");
    // The stub ran (launch happened after registration).
    assert!(std::fs::metadata(format!("{}.cwd", record.display())).is_ok());
    // $CODEX_HOME/config.toml gained the ironmem MCP server block.
    let cfg = std::fs::read_to_string(codex_home.join("config.toml"))
        .expect("launcher should create $CODEX_HOME/config.toml");
    assert!(
        cfg.contains("[mcp_servers.ironmem]"),
        "missing block: {cfg}"
    );
    assert!(cfg.contains("args = [\"serve\"]"), "missing args: {cfg}");
    assert!(
        cfg.contains("IRONMEM_MCP_MODE = \"trusted\""),
        "missing env: {cfg}"
    );
}

#[test]
fn claude_launcher_is_idempotent_across_two_runs() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let db_path = temp.path().join("memory.sqlite3");
    let bin_dir = temp.path().join("bin");
    let repo = temp.path().join("repo");
    let record = temp.path().join("rec");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&bin_dir).unwrap();
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("README.md"), "# repo").unwrap();
    write_stub(&bin_dir, "claude", &record, 0);

    let first = launcher_command(&home, &db_path, &bin_dir)
        .arg("claude")
        .arg(&repo)
        .output()
        .unwrap();
    assert!(first.status.success());

    let second = launcher_command(&home, &db_path, &bin_dir)
        .arg("claude")
        .arg(&repo)
        .output()
        .unwrap();
    assert!(second.status.success());
    let stderr2 = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr2.contains("already registered"),
        "2nd run should report already-registered: {stderr2}"
    );

    // Exactly one ironmem entry remains.
    let cfg = std::fs::read_to_string(home.join(".claude.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&cfg).unwrap();
    assert!(v["mcpServers"]["ironmem"].is_object());
}
