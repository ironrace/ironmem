//! Shared git subprocess environment controls. No MCP or Autopilot dependencies.

use std::process::Command;

/// Strip every inherited `GIT_*` variable before spawning git.
///
/// `git -C <path>` is not enough on its own: an inherited `GIT_DIR` /
/// `GIT_WORK_TREE` (and friends) silently redirects the command at a
/// *different* repository, so a checkpoint would be compared against the wrong
/// HEAD — and could be reported `head_check: matches` while the session's real
/// repo has drifted. That is an unverified claim presented as verified, the
/// exact failure issue #273 exists to end, arriving through the environment
/// rather than through a stale record.
///
/// Not hypothetical, and not novel here: `review::diff`'s helper of the same
/// name says these overrides "can redirect a command away from `request.repo`",
/// the review hook does the same, and this file's own test fixtures already
/// scrub — the production path was the one left exposed. PATH and the rest of
/// the environment are deliberately preserved; only git's process controls go.
///
/// `pub(crate)` so every git shell-out in the crate goes through the same
/// scrub rather than growing a second, unscrubbed spelling — `collab_checkpoint`'s
/// inspection and range-verification shell-outs (Task 10), and `code_maps`'
/// worktree probe and freshness diff. Every `Command::new("git")` in the crate
/// must call it before `.output()`.
///
/// Widened from `pub(super)` because the two `code_maps` spawns were the
/// remaining exposure, and it was not theoretical: this module's test
/// fixtures once set `GIT_DIR`/`GIT_WORK_TREE` process-wide, and `cargo test`
/// runs the lib binary's tests on parallel threads — so an unscrubbed
/// `git rev-parse --show-toplevel` beside them intermittently resolved to the
/// fixture's repo instead of its own `current_dir`, failing two `code_maps`
/// tests at random. No test sets a `GIT_*` variable on the process any more
/// (`git_env_scan` fails if one does); the hostile-environment tests put it
/// on the child ([`scrubbed_git`]).
///
/// The removal is recorded in two passes, and the first one is what makes the
/// guarantee unconditional. `Command` applies recorded removals against the
/// environment as it exists at *spawn* time, so a sweep of `vars_os()` alone
/// only strips what happened to be set at scrub time — a `GIT_DIR` that
/// appears between the sweep and `.output()` is inherited. `env_remove`
/// records the removal whether or not the variable is currently set, so
/// naming the redirecting variables outright ([`REDIRECTING_GIT_VARS`])
/// closes that window. The sweep still runs afterward, for anything outside
/// that list that is already set.
pub(crate) fn scrub_git_environment(command: &mut Command) {
    for key in REDIRECTING_GIT_VARS {
        command.env_remove(key);
    }
    for (key, _) in std::env::vars_os() {
        if key
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(key);
        }
    }
}

/// The `GIT_*` variables that decide *which* repository or configuration a
/// command reads — the ones whose inheritance is a correctness problem rather
/// than a cosmetic one. Removed unconditionally by [`scrub_git_environment`]
/// so the removal is in effect at spawn time no matter when the variable is
/// set. `GIT_CONFIG_COUNT` covers the whole `GIT_CONFIG_KEY_<n>` /
/// `GIT_CONFIG_VALUE_<n>` family: git ignores those without the count, and the
/// family is unbounded, so it cannot be enumerated here.
pub(crate) const REDIRECTING_GIT_VARS: [&str; 15] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
    "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    "GIT_PREFIX",
    "GIT_CONFIG",
    "GIT_CONFIG_GLOBAL",
    "GIT_CONFIG_SYSTEM",
    "GIT_CONFIG_NOSYSTEM",
    "GIT_CONFIG_COUNT",
];

/// A scrubbed git command, for shell-outs whose tests need to show the scrub
/// works: `inherited` variables set on it first, where an inherited
/// environment would reach the child (a test's hostile `GIT_DIR`; empty in
/// production), then [`scrub_git_environment`]. So a test proves the
/// production scrub removes what an inherited environment carries without
/// ever setting a variable on the whole process, which would redirect every
/// other test thread's git.
///
/// Every `inherited` key must be one of [`REDIRECTING_GIT_VARS`], which the
/// scrub removes; any other panics. The seam can then never put anything on
/// production git, such as `GIT_SSH_COMMAND`, `GIT_EXEC_PATH` or `PATH`.
pub(crate) fn scrubbed_git(inherited: &[(&str, &std::ffi::OsStr)]) -> Command {
    for (key, _) in inherited {
        assert!(
            REDIRECTING_GIT_VARS.contains(key),
            "scrubbed_git only takes the redirecting git variables its scrub removes, not `{key}`"
        );
    }
    let mut command = Command::new("git");
    command.envs(inherited.iter().copied());
    scrub_git_environment(&mut command);
    command
}
