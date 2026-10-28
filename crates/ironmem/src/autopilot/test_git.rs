//! Git for Autopilot's unit-test fixtures, and only for them.
//!
//! Every fixture git call goes through [`command`]: it removes every
//! inherited `GIT_*` variable (an outer `GIT_DIR` would aim it at another
//! repository), then points git at no global or system config. The
//! operator's global config may sign commits with their key, run their
//! hooks, or change a default a test relies on; a fixture that read it could
//! pass locally and fail in CI, or sign a throwaway commit with a real key.
//!
//! Production git is never built here: the code under test reads the config
//! its callers set up, and that is part of what the tests check.

use std::path::Path;
use std::process::{Command, Output};

/// A fixture's git: scrubbed, then reading no global or system config.
pub(crate) fn command() -> Command {
    let mut command = Command::new("git");
    crate::mcp::tools::scrub_git_environment(&mut command);
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    command
}

/// Run `args` in `dir`, asserting that git succeeds.
pub(crate) fn output(dir: &Path, args: &[&str]) -> Output {
    let output = command()
        .args(args)
        .current_dir(dir)
        .output()
        .expect("git should be runnable in tests");
    assert!(
        output.status.success(),
        "git {} failed in {}: {}",
        args.join(" "),
        dir.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

/// [`output`]'s stdout, trimmed.
pub(crate) fn run(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&output(dir, args).stdout)
        .trim()
        .to_string()
}

/// `git config --get key` in `dir`: `None` when unset (exit 1); any other
/// failure fails the test.
pub(crate) fn config_get(dir: &Path, key: &str) -> Option<String> {
    let out = command()
        .args(["config", "--get", key])
        .current_dir(dir)
        .output()
        .expect("git should be runnable in tests");
    match out.status.code() {
        Some(0) => Some(String::from_utf8_lossy(&out.stdout).trim().to_string()),
        Some(1) => None,
        code => panic!(
            "git config --get {key} failed in {} ({code:?}): {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

/// Whether `rev` names an object in `dir` (`rev-parse --verify --quiet`):
/// exit 1 is "no"; any other failure fails the test.
pub(crate) fn verifies(dir: &Path, rev: &str) -> bool {
    let out = command()
        .args(["rev-parse", "--verify", "--quiet", rev])
        .current_dir(dir)
        .output()
        .expect("git should be runnable in tests");
    match out.status.code() {
        Some(0) => true,
        Some(1) => false,
        code => panic!(
            "git rev-parse --verify {rev} failed in {} ({code:?}): {}",
            dir.display(),
            String::from_utf8_lossy(&out.stderr)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixture_git_reads_no_global_or_system_config() {
        let built = command();
        let envs: Vec<_> = built.get_envs().collect();
        for (key, value) in [
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ] {
            assert!(
                envs.contains(&(std::ffi::OsStr::new(key), Some(std::ffi::OsStr::new(value)))),
                "{key}: {envs:?}"
            );
        }
        // And git honours it: nothing is read from a global config.
        let dir = tempfile::tempdir().unwrap();
        let global = command()
            .args(["config", "--global", "--list"])
            .current_dir(dir.path())
            .output()
            .unwrap();
        assert!(global.stdout.is_empty(), "{global:?}");
    }

    #[test]
    #[should_panic(expected = "git rev-parse --no-such-flag failed")]
    fn a_failing_fixture_git_call_fails_the_test() {
        let dir = tempfile::tempdir().unwrap();
        run(dir.path(), &["rev-parse", "--no-such-flag"]);
    }
}
