//! Shared `#[cfg(test)]` fixtures for `mcp::tools` unit tests.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use crate::config::{Config, EmbedMode, McpAccessMode};

pub(crate) fn test_app_with_db_path(db_path: PathBuf, root: &Path) -> Arc<crate::mcp::app::App> {
    let config = Config {
        db_path,
        model_dir: root.join("model"),
        model_dir_explicit: true,
        state_dir: root.join("state"),
        mcp_access_mode: McpAccessMode::Trusted,
        embed_mode: EmbedMode::Noop,
    };
    #[allow(clippy::arc_with_non_send_sync)]
    Arc::new(crate::mcp::app::App::new(config).unwrap())
}

/// Run `git` in `cwd` with every inherited `GIT_*` environment variable
/// scrubbed first — an inherited `GIT_DIR`/`GIT_WORK_TREE` would otherwise
/// make this operate on (or report shas from) a different repo than the one
/// at `cwd`, silently. Asserts success so a broken fixture fails loudly at
/// setup rather than producing a confusing downstream git error.
fn run_git(cwd: &Path, args: &[&str]) -> std::process::Output {
    let mut command = Command::new("git");
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
    let output = command
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git must run");
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// A real git repo, isolated from this machine's global git config —
/// `commit.gpgsign` and `core.hooksPath` are pinned off explicitly rather
/// than inherited, since a developer machine with a working signing key
/// silently masks what would hang or fail on a CI runner with neither
/// configured — with `n` sequential commits, each a real descendant of the
/// one before. Ancestry validation (issue #273 Task 8) needs every
/// batch-flow test that drives a session past `CodeImplementPending` to
/// report a real, reachable commit instead of the historical placeholder
/// `"/tmp/repo"` and synthetic heads like `"c1"`/`"c2"`.
///
/// Named distinctly from `mcp_protocol.rs`'s integration-test fixture of the
/// same shape (`git_batch_repo`) — the two cannot share code across the
/// crate/integration-test boundary, but they should not share a name either.
pub(crate) fn git_ancestor_chain(n: usize) -> (tempfile::TempDir, String, Vec<String>) {
    let temp = tempfile::tempdir().expect("temp repo must be creatable");
    let repo_path = temp.path().to_path_buf();
    run_git(&repo_path, &["init"]);
    run_git(&repo_path, &["config", "user.name", "Ironmem Test"]);
    run_git(&repo_path, &["config", "user.email", "ironmem@example.com"]);
    run_git(&repo_path, &["config", "commit.gpgsign", "false"]);
    run_git(&repo_path, &["config", "core.hooksPath", "/dev/null"]);
    let mut shas = Vec::with_capacity(n);
    for i in 0..n {
        std::fs::write(repo_path.join("batch.txt"), format!("v{i}\n"))
            .expect("fixture file must be writable");
        run_git(&repo_path, &["add", "batch.txt"]);
        run_git(&repo_path, &["commit", "-m", &format!("commit {i}")]);
        let output = run_git(&repo_path, &["rev-parse", "HEAD"]);
        shas.push(
            String::from_utf8(output.stdout)
                .expect("sha must be utf-8")
                .trim()
                .to_string(),
        );
    }
    (temp, repo_path.to_string_lossy().into_owned(), shas)
}
