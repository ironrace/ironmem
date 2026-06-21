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
/// `<record>.cwd` and each argv element (one per line) to `<record>.args`.
fn write_stub(dir: &Path, name: &str, record: &Path) -> PathBuf {
    let stub = dir.join(name);
    let script = format!(
        "#!/bin/sh\npwd > \"{rec}.cwd\"\n: > \"{rec}.args\"\nfor a in \"$@\"; do printf '%s\\n' \"$a\" >> \"{rec}.args\"; done\nexit 0\n",
        rec = record.display()
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
    write_stub(&bin_dir, "claude", &record);

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
