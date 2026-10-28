//! Source scans that keep git's environment out of every test thread's reach.
//!
//! `cargo test` runs a binary's tests on parallel threads that share one
//! process environment. A test that sets `GIT_DIR` (or any `GIT_*`) on the
//! whole process redirects every git command another thread spawns while it
//! is set, and a git helper that removes only the `GIT_*` variables already
//! set when it builds its command still inherits one set between that check
//! and the spawn. That race failed
//! `full_recovery_path_through_tool_level_turn_gate_clears_on_delegated_completion`
//! on Linux CI. These scans fail on either shape coming back, before it
//! flakes:
//!
//! - no test (under `src/` or `tests/`) sets or removes a `GIT_*` variable on
//!   the process; a test that needs one puts it on the child `Command`;
//! - every `Command::new("git")` under `src/` is bound with `let mut <name>`
//!   and followed, within a few lines, by `scrub_git_environment(&mut
//!   <name>)`, the one shared scrub, which removes every redirecting variable
//!   unconditionally.
//!
//! They read the source rather than spawn anything: a call site that forgot
//! is invisible at runtime until the day a variable is set beside it.
//!
//! The first is a pattern match over statements, with known limits: it sees
//! a `"GIT_…"` literal only in the statement that writes it, so a key held in
//! an earlier binding or a `const` slips past, and it knows only the writers
//! it names (`set_var`, `remove_var`, `Scoped…`, `EnvGuard`, `with_env`,
//! `temp_env`, `with_var`), so a helper under another name slips past too.

use std::path::{Path, PathBuf};

/// This file names the patterns it scans for.
const SELF: &str = "git_env_scan.rs";

/// How many lines after `Command::new("git")` the scrub may come.
const SCRUB_WINDOW: usize = 6;

/// Every `.rs` file under `dir`, recursively.
fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(dir) = pending.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs")
                && path.file_name().is_some_and(|name| name != SELF)
            {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

fn crate_dir(sub: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(sub)
}

fn is_comment(line: &str) -> bool {
    line.trim_start().starts_with("//")
}

/// Statements (split on `;`) that name a `GIT_*` variable as a string
/// literal and change the process environment: `std::env::set_var`,
/// `remove_var`, or a scoped-environment helper (`Scoped…`, `EnvGuard`,
/// `with_env`, `temp_env`, `with_var`) called with it. A `Command`'s own
/// `.env(…)`/`.env_remove(…)` and reading a variable are fine.
fn process_git_env_writes(source: &str) -> Vec<String> {
    let code: String = source
        .lines()
        .filter(|line| !is_comment(line))
        .collect::<Vec<_>>()
        .join("\n");
    code.split(';')
        .filter(|statement| statement.contains("\"GIT_"))
        .filter(|statement| {
            [
                "set_var(",
                "remove_var(",
                "Scoped",
                "EnvGuard",
                "with_env",
                "temp_env",
                "with_var",
            ]
            .iter()
            .any(|writer| statement.contains(writer))
        })
        .map(|statement| {
            statement
                .trim()
                .lines()
                .take(3)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[test]
fn no_test_sets_a_git_variable_on_the_whole_process() {
    let mut found = Vec::new();
    for dir in ["src", "tests"] {
        for file in rust_files(&crate_dir(dir)) {
            let source = std::fs::read_to_string(&file).unwrap();
            for statement in process_git_env_writes(&source) {
                found.push(format!("{}: {statement}", file.display()));
            }
        }
    }
    assert!(
        found.is_empty(),
        "these set or remove a GIT_* variable on the whole process, which redirects \
         every other test thread's git; put it on the child Command instead:\n{}",
        found.join("\n")
    );
}

/// The `<name>` a line binds with `let mut <name> =`, if it does.
fn mutable_binding(line: &str) -> Option<&str> {
    let rest = line.split_once("let mut ")?.1;
    let end = rest
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    (end > 0 && rest[end..].trim_start().starts_with(['=', ':'])).then(|| &rest[..end])
}

/// `Command::new("git")` sites (one-based lines) under `src/` that are not
/// bound to a `let mut <name>` scrubbed by `scrub_git_environment(&mut
/// <name>)` within [`SCRUB_WINDOW`] lines: a scrub of another binding in the
/// window does not count.
fn unscrubbed_git_commands(source: &str) -> Vec<usize> {
    let lines: Vec<&str> = source.lines().collect();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| !is_comment(line) && line.contains("Command::new(\"git\")"))
        .filter(|(at, line)| {
            let Some(name) = mutable_binding(line) else {
                return true;
            };
            let scrub = format!("scrub_git_environment(&mut {name})");
            !lines[*at..lines.len().min(at + SCRUB_WINDOW + 1)]
                .iter()
                .any(|line| !is_comment(line) && line.contains(&scrub))
        })
        .map(|(at, _)| at + 1)
        .collect()
}

#[test]
fn every_git_command_is_scrubbed_by_the_one_shared_scrub() {
    let mut unscrubbed = Vec::new();
    let mut definitions = Vec::new();
    for file in rust_files(&crate_dir("src")) {
        let source = std::fs::read_to_string(&file).unwrap();
        for line in unscrubbed_git_commands(&source) {
            unscrubbed.push(format!("{}:{line}", file.display()));
        }
        if source
            .lines()
            .any(|line| !is_comment(line) && line.contains("fn scrub_git_environment("))
        {
            definitions.push(file.display().to_string());
        }
    }
    assert!(
        unscrubbed.is_empty(),
        "these git commands are not bound with `let mut <name>` and scrubbed by \
         `scrub_git_environment(&mut <name>)` within {SCRUB_WINDOW} lines, so an \
         inherited GIT_DIR can redirect them:\n{}",
        unscrubbed.join("\n")
    );
    // A local function of the same name would satisfy the scan above while
    // scrubbing less: the one in `mcp::tools` removes the redirecting
    // variables unconditionally, which a sweep of what is set at build time
    // does not.
    assert_eq!(
        definitions,
        vec![crate_dir("src/mcp/tools/collab_session.rs")
            .display()
            .to_string()],
        "`scrub_git_environment` must be defined once, in mcp::tools"
    );
}

#[test]
fn the_scans_find_what_they_look_for() {
    let writes = process_git_env_writes(
        "let _a = ScopedEnvVar::set(\"GIT_DIR\", x);\n\
         std::env::set_var(\"GIT_WORK_TREE\", y);\n\
         for key in [\"GIT_DIR\"] { std::env::remove_var(key); }\n\
         command.env(\"GIT_DIR\", z);\n\
         let email = std::env::var(\"GIT_COMMITTER_EMAIL\");\n\
         // std::env::set_var(\"GIT_DIR\", w);\n\
         temp_env::with_var(\"GIT_DIR\", Some(v), || run());\n",
    );
    assert_eq!(writes.len(), 4, "{writes:?}");
    let unscrubbed = unscrubbed_git_commands(&format!(
        "let a = Command::new(\"git\");\n{}\
         let mut b = Command::new(\"git\");\n\
         // scrub_git_environment(&mut a);\n\
         scrub_git_environment(&mut b);\n",
        "a.arg(\"x\");\n".repeat(SCRUB_WINDOW)
    ));
    assert_eq!(unscrubbed, vec![1]);
    // A scrub in the window counts only for the command it scrubs: `a` is
    // not scrubbed by `b`'s, nor is a command with no binding to name.
    let unscrubbed = unscrubbed_git_commands(
        "let mut a = Command::new(\"git\");\n\
         let mut b = Command::new(\"git\");\n\
         scrub_git_environment(&mut b);\n\
         Command::new(\"git\").arg(\"x\").output();\n\
         scrub_git_environment(&mut b);\n\
         let mut c = std::process::Command::new(\"git\");\n\
         crate::mcp::tools::scrub_git_environment(&mut c);\n",
    );
    assert_eq!(unscrubbed, vec![1, 4]);
}

/// The integration tests cannot import the lib's private
/// `REDIRECTING_GIT_VARS`, so `tests/git_env/mod.rs` keeps a copy every
/// integration git helper scrubs with. It must name exactly the same
/// variables, or a helper would leave one of them to be inherited.
#[test]
fn the_integration_tests_scrub_the_same_redirecting_variables() {
    let source = std::fs::read_to_string(crate_dir("tests/git_env/mod.rs"))
        .expect("tests/git_env/mod.rs holds the integration tests' git scrub");
    let list = source
        .split_once("REDIRECTING_GIT_VARS")
        .and_then(|(_, rest)| rest.split_once("= ["))
        .and_then(|(_, rest)| rest.split_once("];"))
        .map(|(list, _)| list)
        .expect("a REDIRECTING_GIT_VARS array");
    let copied: Vec<&str> = list.split('"').skip(1).step_by(2).collect();
    assert_eq!(copied, crate::mcp::tools::REDIRECTING_GIT_VARS);
}
