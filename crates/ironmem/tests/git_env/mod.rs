//! Git's environment, removed from the commands the integration test
//! binaries spawn. Each binary that needs it declares `mod git_env;`
//! (`autopilot_support` reaches it by `#[path]`).
//!
//! `#![allow(dead_code)]`: each binary compiles its own copy, and none uses
//! every item.

#![allow(dead_code)]

use std::process::Command;

/// The `GIT_*` variables that decide which repository or configuration git
/// reads. A copy of the lib's private `mcp::tools::REDIRECTING_GIT_VARS`,
/// which integration tests cannot import;
/// `git_env_scan::the_integration_tests_scrub_the_same_redirecting_variables`
/// fails if the two differ.
pub const REDIRECTING_GIT_VARS: [&str; 15] = [
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

/// Remove git's environment from `command`. The redirecting variables go
/// unconditionally, so one set on the process after this call is not
/// inherited either (`Command` applies removals at spawn); the sweep then
/// removes any other `GIT_*` set now.
pub fn scrub_git_env(command: &mut Command) -> &mut Command {
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
    command
}

/// [`scrub_git_env`] for a fixture's own git, which also reads no global or
/// system config: the operator's may sign commits with their key, run their
/// hooks, or change a default a fixture relies on. Never for the ironmem
/// binary under test, whose git behaviour is what the tests check.
pub fn fixture_git_env(command: &mut Command) -> &mut Command {
    scrub_git_env(command)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}
