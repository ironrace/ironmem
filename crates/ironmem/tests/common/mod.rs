//! Fixtures shared by the collab integration-test binaries.
//!
//! Rust compiles every file in `tests/` into its own binary, so a helper
//! defined in `mcp_protocol.rs` is unreachable from
//! `collab_checkpoint_consistency.rs`. Both drive the same protocol surface
//! against the same git fixtures, and issue #273's whole subject is a record
//! and a repo that quietly disagree — so a *second copy* of the fixtures is
//! the last thing this area needs. Two copies of `git_batch_repo` that drift
//! apart would let the end-to-end regression tests pass against a repo shaped
//! differently from the one the protocol tests exercise, which is the same
//! class of failure (two artifacts asserted to describe one thing) one level
//! down.
//!
//! So the closure is extracted here once and both binaries `mod common;` it.
//! `mcp_protocol.rs` no longer defines these; it uses these.
//!
//! `#![allow(dead_code)]`: a `tests/common` module is compiled separately into
//! each binary that includes it, and neither binary uses every helper. Without
//! this, `cargo clippy --all-targets -- -D warnings` fails on the helpers the
//! *other* binary needs. This is the standard idiom for shared test support,
//! and the alternative — pruning to the intersection — would put the fixtures
//! back where they started.
#![allow(dead_code)]

use ironmem::mcp::app::App;
use ironmem::mcp::protocol::JsonRpcRequest;
use ironmem::mcp::server::dispatch;
use serde_json::json;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run `git`, first scrubbing every inherited `GIT_*` environment variable —
/// an inherited `GIT_DIR`/`GIT_WORK_TREE` would otherwise make `git` operate
/// on (or report shas from) a different repo than the fixture at `cwd`,
/// silently. Same idiom as `review_diff.rs`'s `scrub_git_environment`.
pub(crate) fn git(args: &[&str], cwd: &Path) -> String {
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
        .expect("git command must run");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git output must be valid utf-8")
        .trim()
        .to_string()
}

pub(crate) fn write_file(path: &Path, contents: &str) {
    std::fs::write(path, contents).expect("fixture file must be writable");
}

pub(crate) fn commit_file(cwd: &Path, filename: &str, contents: &str, message: &str) -> String {
    write_file(&cwd.join(filename), contents);
    git(&["add", filename], cwd);
    git(&["commit", "-m", message], cwd);
    git(&["rev-parse", "HEAD"], cwd)
}

pub(crate) fn request(method: &str, params: serde_json::Value) -> JsonRpcRequest {
    serde_json::from_value(json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": method,
        "params": params,
    }))
    .expect("request fixture must deserialize")
}

/// Call a tool and return its parsed JSON result, **failing the test if the
/// tool refused**.
///
/// The `isError` assertion is not belt-and-braces. A tool refusal does not
/// come back as the JSON-RPC `error` field — it is an `isError: true` success
/// response carrying `{"error": "..."}` as its content text, exactly as
/// [`call_tool_expect_error`] below documents. Without the check this helper
/// parsed that payload and handed it back as an ordinary value, so any call
/// site that did not go on to assert something about the *shape* of what it
/// got passed silently on a refused call — and a setup step is precisely the
/// call site that asserts nothing. Verified in practice: a `collab_recv`
/// fixture that passed `agent` where the tool wants `receiver` refused with
/// "receiver is required", and the test carried on and asserted its way to a
/// wrong conclusion about the *next* call.
pub(crate) fn call_tool(app: &App, name: &str, args: serde_json::Value) -> serde_json::Value {
    let req = request("tools/call", json!({ "name": name, "arguments": args }));
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    assert!(resp.error.is_none(), "unexpected RPC error for tool {name}");
    let result = resp.result.unwrap();
    let text = result["content"][0]["text"]
        .as_str()
        .expect("content[0].text must be a string");
    assert_ne!(
        result["isError"],
        json!(true),
        "tool {name} refused, but this call site expected success: {text}. \
         If the refusal is what the test is about, use call_tool_expect_error."
    );
    serde_json::from_str(text).expect("tool response text must be valid JSON")
}

/// Tool errors surface as an `isError: true` success response carrying a
/// JSON error string, not as the JSON-RPC `error` field. Return the error
/// message so callers can assert on its contents.
pub(crate) fn call_tool_expect_error(app: &App, name: &str, args: serde_json::Value) -> String {
    let req = request("tools/call", json!({ "name": name, "arguments": args }));
    let resp = dispatch(app, &req).expect("tools/call must return a response");
    let result = resp.result.expect("tool result must be present");
    assert_eq!(
        result["isError"], true,
        "expected tool error for {name}, got success: {result}"
    );
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    let parsed: serde_json::Value = serde_json::from_str(text).unwrap_or(json!({}));
    parsed["error"].as_str().unwrap_or(text).to_string()
}

/// Drive a fresh session all the way to PlanLocked via MCP handlers and
/// return `(session_id, final_plan_text)` so callers can assemble valid
/// `task_list` payloads (the state machine rejects a mismatched `plan_hash`).
pub(crate) fn drive_to_plan_locked(app: &App, final_plan: &str) -> String {
    drive_to_plan_locked_with_implementer(app, final_plan, None)
}

/// Same as `drive_to_plan_locked` but threads through the optional
/// `implementer` field. `None` keeps the historical default (`"claude"`).
pub(crate) fn drive_to_plan_locked_with_implementer(
    app: &App,
    final_plan: &str,
    implementer: Option<&str>,
) -> String {
    drive_to_plan_locked_full(app, final_plan, implementer, "/repo")
}

/// Same as `drive_to_plan_locked`, but seeded at a real `repo_path` instead
/// of the historical `"/repo"` placeholder. Every test that drives a session
/// past `CodeImplementPending` needs one of these now (issue #273 Task 8):
/// `implementation_done`/`review_fix_global`/`review_local`/`final_review`
/// are all git-ancestry-checked against `repo_path`, and a placeholder path
/// that resolves to nothing makes that check an operational failure rather
/// than the real refusal (or real success) the test means to exercise.
pub(crate) fn drive_to_plan_locked_in_repo(
    app: &App,
    final_plan: &str,
    repo_path: &Path,
) -> String {
    drive_to_plan_locked_full(app, final_plan, None, &repo_path.to_string_lossy())
}

pub(crate) fn drive_to_plan_locked_full(
    app: &App,
    final_plan: &str,
    implementer: Option<&str>,
    repo_path: &str,
) -> String {
    let mut start_args = json!({
        "repo_path": repo_path,
        "branch": "main",
        "initiator": "claude"
    });
    if let Some(value) = implementer {
        start_args["implementer"] = json!(value);
    }
    let started = call_tool(app, "collab_start", start_args);
    let session_id = started["session_id"].as_str().unwrap().to_string();

    for (sender, content) in [("claude", "cdraft"), ("codex", "xdraft")] {
        call_tool(
            app,
            "collab_send",
            json!({
                "session_id": session_id,
                "sender": sender,
                "topic": "draft",
                "content": content
            }),
        );
    }
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "canonical",
            "content": "canonical plan v1"
        }),
    );
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review",
            "content": json!({ "verdict": "approve" }).to_string()
        }),
    );
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final",
            "content": json!({ "plan": final_plan }).to_string()
        }),
    );
    let status = call_tool(app, "collab_status", json!({ "session_id": &session_id }));
    assert_eq!(status["phase"], "PlanLocked");
    session_id
}

pub(crate) fn plan_hash(app: &App, session_id: &str) -> String {
    let status = call_tool(app, "collab_status", json!({ "session_id": session_id }));
    status["final_plan_hash"].as_str().unwrap().to_string()
}

pub(crate) fn task_list_payload(
    plan_hash: &str,
    base_sha: &str,
    head_sha: &str,
    n: usize,
) -> String {
    let tasks: Vec<_> = (1..=n)
        .map(|i| {
            json!({
                "id": i,
                "title": format!("task {i}"),
                "acceptance": [format!("criterion {i}")]
            })
        })
        .collect();
    json!({
        "plan_hash": plan_hash,
        "base_sha": base_sha,
        "head_sha": head_sha,
        "tasks": tasks,
    })
    .to_string()
}

/// A checkpoint payload that satisfies every condition of the
/// `implementation_done` gate for a batch of `tasks` tasks at `head`.
pub(crate) fn batch_complete_checkpoint(
    session_id: &str,
    agent: &str,
    head: &str,
    tasks: u64,
) -> serde_json::Value {
    let completed = (1..=tasks)
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",");
    json!({
        "session_id": session_id,
        "agent": agent,
        "status": "batch_complete",
        "head_sha": head,
        "completed_task_ids": completed,
        "gates_result": "passed",
        "gates_sha": head,
        "gates_commands": "cargo test --workspace",
    })
}

/// File the checkpoint `implementation_done` now demands as proof (issue #273
/// Task 7), covering every task in the session's accepted task list.
///
/// The task count is read back from `collab_status` — the same single source
/// of truth the gate derives it from — so a test that changes its task list
/// cannot silently file a checkpoint that under-covers and then be refused for
/// a reason it was not written to exercise.
pub(crate) fn checkpoint_batch_complete(app: &App, session_id: &str, agent: &str, head: &str) {
    let status = call_tool(app, "collab_status", json!({ "session_id": session_id }));
    let tasks = status["tasks_count"]
        .as_u64()
        .expect("a session at CodeImplementPending has a readable tasks_count");
    call_tool(
        app,
        "collab_checkpoint",
        batch_complete_checkpoint(session_id, agent, head, tasks),
    );
}

/// Number of `wal_log` rows for the given `operation` whose `params` blob
/// mentions this `session_id`. Rejection tests use this to prove a refused
/// call wrote nothing to the audit trail, not merely that it returned an
/// error.
pub(crate) fn wal_row_count(app: &App, session_id: &str, operation: &str) -> i64 {
    let pattern = format!("%\"session_id\":\"{session_id}\"%");
    app.db
        .with_transaction(|tx| {
            Ok(tx.query_row(
                "SELECT COUNT(*) FROM wal_log WHERE operation = ?1 AND params LIKE ?2",
                rusqlite::params![operation, pattern],
                |row| row.get(0),
            )?)
        })
        .unwrap()
}

/// A fresh git repo, isolated from this machine's global git config —
/// `commit.gpgsign` and `core.hooksPath` are pinned off explicitly rather
/// than left to whatever the developer machine running this test happens to
/// have configured. A working SSH/GPG signing key locally makes an inherited
/// `commit.gpgsign=true` invisible here and a silent hang-or-fail on a CI
/// runner with no key configured; an inherited `core.hooksPath` risks running
/// someone's personal hooks against a throwaway fixture repo — with `n`
/// sequential commits, each a real descendant of the one before.
pub(crate) fn git_batch_repo(n: usize) -> (tempfile::TempDir, PathBuf, Vec<String>) {
    let temp = tempfile::tempdir().expect("temp repo must be creatable");
    let repo_path = temp.path().to_path_buf();
    git(&["init"], &repo_path);
    git(&["config", "user.name", "Ironmem Test"], &repo_path);
    git(&["config", "user.email", "ironmem@example.com"], &repo_path);
    git(&["config", "commit.gpgsign", "false"], &repo_path);
    git(&["config", "core.hooksPath", "/dev/null"], &repo_path);
    let shas = (0..n)
        .map(|i| {
            commit_file(
                &repo_path,
                "batch.txt",
                &format!("v{i}\n"),
                &format!("commit {i}"),
            )
        })
        .collect();
    (temp, repo_path, shas)
}

/// An `App` paired with a fresh [`git_batch_repo`].
///
/// **This is the settled shape** (issue #273 Task 9, reconciling Task 8's
/// fixture against the plan's snippets): `test_app_with_git_repo(n_commits)
/// -> (App, TempDir, PathBuf, Vec<String>)`. Call it as
/// `let (app, _temp, repo, shas) = test_app_with_git_repo(1);`.
///
/// The plan's Task 9/10/13 snippets write `let (app, _tmp, repo) =
/// test_app_with_git_repo();` — a no-argument 3-tuple over an empty repo.
/// That shape cannot serve its own callers: every one of them needs a real
/// commit before a session can be driven anywhere (`task_list` carries a
/// `base_sha`/`head_sha` that Task 8 made git-ancestry-checked), so each would
/// have to make one itself and the fixture would be a fixture for nothing.
/// The commit count goes in and the resulting shas come out instead. Later
/// tasks should adapt their call sites to this signature rather than
/// reintroducing the other one.
pub(crate) fn test_app_with_git_repo(
    n_commits: usize,
) -> (App, tempfile::TempDir, PathBuf, Vec<String>) {
    let app = App::open_for_test().unwrap();
    let (temp, repo_path, shas) = git_batch_repo(n_commits);
    (app, temp, repo_path, shas)
}

/// Drive a fresh session in `repo` to `CodeImplementPending` with an accepted
/// `n_tasks`-task task list, seeded at the repo's current HEAD.
///
/// Companion to [`test_app_with_git_repo`], and the second half of the shared
/// batch fixture the plan's Task 9/10/13 snippets call
/// `start_batch_session_in`.
pub(crate) fn start_batch_session_in(app: &App, repo: &Path, n_tasks: usize) -> String {
    let head = git(&["rev-parse", "HEAD"], repo);
    let session_id = drive_to_plan_locked_in_repo(app, "batch plan", repo);
    let hash = plan_hash(app, &session_id);
    call_tool(
        app,
        "collab_send",
        json!({
            "session_id": &session_id,
            "sender": "claude",
            "topic": "task_list",
            "content": task_list_payload(&hash, &head, &head, n_tasks)
        }),
    );
    assert_eq!(phase_of(app, &session_id), "CodeImplementPending");
    session_id
}

/// Same as [`start_batch_session_in`], but driven on to a genuinely
/// tooling-class `CodingFailed` by three successive `git_commit_failed:`
/// reports — the per-resume ceiling is 2, so the third breaks it. Turn
/// ownership alternates claude → codex → claude as each recoverable report
/// hands control to the counterpart, which is why the senders are not all the
/// same agent.
pub(crate) fn failed_batch_session_in(app: &App, repo: &Path, n_tasks: usize) -> String {
    let session_id = start_batch_session_in(app, repo, n_tasks);
    for (sender, attempt) in [("claude", 1), ("codex", 2), ("claude", 3)] {
        call_tool(
            app,
            "collab_send",
            json!({
                "session_id": &session_id,
                "sender": sender,
                "topic": "failure_report",
                "content": json!({
                    "coding_failure": format!("git_commit_failed: attempt {attempt}")
                })
                .to_string()
            }),
        );
    }
    assert_eq!(phase_of(app, &session_id), "CodingFailed");
    session_id
}

/// File a checkpoint through the real `collab_checkpoint` tool. `fields` is
/// merged over `session_id`/`agent` so a caller writes only what it means to
/// exercise.
pub(crate) fn checkpoint(
    app: &App,
    session_id: &str,
    fields: serde_json::Value,
) -> serde_json::Value {
    let mut args = json!({ "session_id": session_id, "agent": "claude" });
    for (key, value) in fields
        .as_object()
        .expect("checkpoint fields must be an object")
    {
        args[key] = value.clone();
    }
    call_tool(app, "collab_checkpoint", args)
}

/// The fenced `handoff_block` from a real `session_handoff` call.
pub(crate) fn handoff_block(app: &App, session_id: &str) -> String {
    call_tool(
        app,
        "session_handoff",
        json!({ "session_id": session_id, "agent": "claude" }),
    )["handoff_block"]
        .as_str()
        .expect("session_handoff must return a handoff_block")
        .to_string()
}

/// Make `repo` unreadable *as a git repo* while leaving the session's
/// `repo_path` pointing at it, so `git rev-parse HEAD` fails the way it would
/// on a worktree that was moved, unmounted, or never checked out.
///
/// This is the input that separates "checked, no drift" from "could not
/// check" — the distinction every surface in issue #273 Task 9 must keep,
/// since an unreadable repo is exactly where a checkpoint is most likely to be
/// stale.
pub(crate) fn break_git_repo(repo: &Path) {
    std::fs::remove_dir_all(repo.join(".git")).expect("fixture .git must be removable");
}

/// `collab_status`'s `phase` field, as an owned `String`. Named to match what
/// issue #273's Task 9 plans to build (`phase_of`) — adopt this rather than
/// duplicating it.
pub(crate) fn phase_of(app: &App, session_id: &str) -> String {
    call_tool(app, "collab_status", json!({ "session_id": session_id }))["phase"]
        .as_str()
        .unwrap()
        .to_string()
}
