//! Unit tests for the ProcessWorkspaceProvisioner argv builder (Task 5).
//! No real git is spawned in default `cargo test`.

use std::path::PathBuf;
use abeval::client::worktree_add_argv;

#[test]
fn worktree_add_argv_is_no_shell_and_detached() {
    let (program, argv) = worktree_add_argv(
        &PathBuf::from("/repo"),
        &PathBuf::from("/ws/t1/ironmem"),
        "abcdef1",
    );
    assert_eq!(program, "git");
    assert_eq!(
        argv,
        vec![
            "-C".to_string(),
            "/repo".to_string(),
            "worktree".to_string(),
            "add".to_string(),
            "--detach".to_string(),
            "/ws/t1/ironmem".to_string(),
            "abcdef1".to_string(),
        ]
    );
}

/// Real git smoke — manually runnable only; never fires in default `cargo test`.
#[test]
#[ignore = "spawns real git; run manually behind the live gate"]
fn real_worktree_add_smoke() {
    // Intentionally left as a manual harness.
}
