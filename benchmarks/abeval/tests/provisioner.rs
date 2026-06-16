//! Unit tests for the ProcessWorkspaceProvisioner helpers and fail-fast guards.
//! No real git is spawned in default `cargo test`.

use abeval::arms::Arm;
use abeval::client::{
    ensure_workspace_path_safe, worktree_add_argv, worktree_parent_dir,
    ProcessWorkspaceProvisioner, ProvisionRequest, WorkspaceProvisioner,
};
use abeval::corpus::Task;
use std::path::PathBuf;

fn task() -> Task {
    Task {
        id: "t1".to_string(),
        title: "T".to_string(),
        source: "issue:#1".to_string(),
        repo_scope: vec!["crates/ironmem/src/lib.rs".to_string()],
        prompt: "p".to_string(),
        acceptance: vec!["a".to_string()],
        gates: vec!["cargo test".to_string()],
        setup_notes: None,
        base_commit: "abcdef1".to_string(),
    }
}

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
            "--".to_string(),
            "/ws/t1/ironmem".to_string(),
            "abcdef1".to_string(),
        ]
    );
}

#[test]
fn worktree_parent_dir_is_task_directory() {
    assert_eq!(
        worktree_parent_dir(&PathBuf::from("/ws/t1/ironmem")),
        Some(PathBuf::from("/ws/t1"))
    );
}

#[test]
fn process_provisioner_rejects_nonempty_workspace_before_git() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_root = temp.path().join("workspaces");
    let workspace = workspace_root.join("t1").join("ironmem");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(workspace.join("marker"), "stale").unwrap();

    let provisioner = ProcessWorkspaceProvisioner {
        ironmem_repo: PathBuf::from("/definitely/not/a/git/repo"),
    };
    let task = task();
    let err = provisioner
        .provision(&ProvisionRequest {
            task: &task,
            arm: Arm::Ironmem,
            base_commit: &task.base_commit,
            workspace_root: &workspace_root,
            workspace: &workspace,
        })
        .unwrap_err()
        .to_string();

    assert!(err.contains("stale worktree"), "got: {err}");
}

#[cfg(unix)]
#[test]
fn workspace_path_safety_rejects_symlinked_arm_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_root = temp.path().join("workspaces");
    let workspace = workspace_root.join("t1").join("ironmem");
    let target = temp.path().join("elsewhere");
    std::fs::create_dir_all(workspace.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, &workspace).unwrap();

    let err = ensure_workspace_path_safe(&workspace_root, &workspace)
        .unwrap_err()
        .to_string();
    assert!(err.contains("symlink"), "got: {err}");
}

/// Real git smoke — manually runnable only; never fires in default `cargo test`.
#[test]
#[ignore = "spawns real git; run manually behind the live gate"]
fn real_worktree_add_smoke() {
    // Intentionally left as a manual harness.
}
