//! Fixtures shared by the Autopilot public-CLI test binaries
//! (`autopilot_cli_implementer.rs`, `autopilot_cli_muse.rs`).
//!
//! Kept apart from `tests/common`, which is the collab and MCP fixture
//! module. Rust compiles each file in `tests/` into its own binary, so each
//! binary that needs these declares `mod autopilot_support;`.
//!
//! `#![allow(dead_code)]`: each binary compiles its own copy, and none uses
//! every helper.

#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ironmem::autopilot::gate_config;
use ironmem::db::schema::Database;

/// The repository every fixture's gate config is approved for.
pub const REPO: &str = "owner/repo";

/// The ironmem binary under test.
pub fn bin() -> &'static str {
    env!("CARGO_BIN_EXE_ironmem")
}

/// Remove every `GIT_*` variable from `command`'s environment, so a
/// `GIT_DIR` set by an outer context (a git hook running `cargo test`) can
/// never point it at another repository.
pub fn scrub_git_env(command: &mut Command) -> &mut Command {
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    command
}

/// The ironmem binary with every `GIT_*` variable stripped, so neither it
/// nor the stub it spawns can reach a repository named by the outer context.
pub fn ironmem() -> Command {
    let mut command = Command::new(bin());
    scrub_git_env(&mut command);
    command
}

/// [`scrub_git_env`] for a fixture's own git, which also reads no global or
/// system config: the operator's may sign commits with their key, run their
/// hooks, or change a default a fixture relies on. Never for the ironmem
/// binary under test ([`ironmem`]), whose git behaviour is what the tests
/// check.
pub fn fixture_git_env(command: &mut Command) -> &mut Command {
    scrub_git_env(command)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}

/// Run fixture git in `dir` ([`fixture_git_env`]), asserting it succeeds.
pub fn git(dir: &Path, args: &[&str]) -> Output {
    let mut command = Command::new("git");
    fixture_git_env(&mut command);
    let output = command.args(args).current_dir(dir).output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A repository with one commit on `main`, signing and hooks pinned off.
pub fn make_repo(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git(path, &["init", "-q", "--initial-branch=main"]);
    git(path, &["config", "user.email", "test@example.com"]);
    git(path, &["config", "user.name", "Test"]);
    // Pinned: the global config signs and runs hooks; worktrees share this.
    git(path, &["config", "commit.gpgsign", "false"]);
    git(path, &["config", "core.hooksPath", "/dev/null"]);
    std::fs::write(path.join("README.md"), "seed\n").unwrap();
    git(path, &["add", "README.md"]);
    git(path, &["commit", "-qm", "seed"]);
}

/// Approve a gate config of `true` for [`REPO`] in the database at `db_path`.
pub fn approve(db_path: &Path) {
    let db = Database::open(db_path).unwrap();
    db.migrate().unwrap();
    gate_config::propose_gate_config(&db, REPO, vec!["true".into()], Vec::new()).unwrap();
    gate_config::approve_gate_config(&db, REPO).unwrap();
}

/// Write `body` to `path` and make it executable.
pub fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// A PATH of `stubs` and the system directories only, so no real harness
/// binary can be found.
pub fn isolated_path(stubs: &Path) -> PathBuf {
    let mut paths = vec![stubs.to_path_buf()];
    paths.extend(
        ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
            .into_iter()
            .map(PathBuf::from),
    );
    std::env::join_paths(paths).unwrap().into()
}
