# Collab Coding-Review Shortcut Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a `collab_start_code_review` MCP tool + `/collab review` slash-command subcommand that creates an IronRace Collab session positioned directly in the v3 global-review stage (`CodeReviewFixGlobalPending`, owner `codex`), skipping v1 planning and v3 per-task coding entirely.

**Architecture:** Additive only. No existing phases, events, state transitions, or topic semantics change. One new state-machine entry point (`start_global_review_session`) seeds a fresh `CollabSession` directly in `CodeReviewFixGlobalPending` with `base_sha`/`last_head_sha` pre-populated. One new MCP handler and schema entry wire it into the tool registry. From that point onward the existing `CodeReviewFixGlobal` → `FinalReview` → `CodingComplete` transitions run unchanged.

**Tech Stack:** Rust, SQLite (via `rusqlite`), serde_json, `uuid`, existing `crate::collab::*` and `crate::mcp::*` modules. Spec: `docs/superpowers/specs/2026-04-22-collab-coding-review-shortcut-design.md`. Protocol: `docs/COLLAB.md`.

---

## File Structure

**Create:**
- None.

**Modify:**
- `crates/ironmem/src/collab/session.rs` — add constructor for review-only sessions (`CollabSession::new_global_review`).
- `crates/ironmem/src/collab/state_machine/mod.rs` — add `start_global_review_session` helper that validates inputs and builds the seeded session.
- `crates/ironmem/src/collab/mod.rs` — re-export new helper if needed (match existing re-export pattern).
- `crates/ironmem/src/mcp/tools/collab_session.rs` — add `handle_collab_start_code_review` handler.
- `crates/ironmem/src/mcp/tools/mod.rs` — register tool schema, dispatch, known-list, mode-gating (write tool).
- `docs/COLLAB.md` — add "Shortcut: post-subagent coding review" subsection and list the new MCP tool.
- `.claude-plugin/commands/collab.md` — add `/collab review <short-topic>` subcommand.
- `.codex-plugin/prompts/collab.md` — mirror the `/collab review` flow on the Codex side.

**Test:**
- `crates/ironmem/src/collab/state_machine/tests.rs` — unit tests for `start_global_review_session` and its invariants.
- `crates/ironmem/tests/mcp_protocol.rs` — end-to-end tests for `collab_start_code_review` through `FinalReview` → `CodingComplete`.

---

## Task 1: Add `CollabSession::new_global_review` constructor

**Files:**
- Modify: `crates/ironmem/src/collab/session.rs`
- Test: `crates/ironmem/src/collab/session.rs` (module `#[cfg(test)]` if one exists; otherwise put the test inline in `crates/ironmem/src/collab/state_machine/tests.rs` in Task 2)

- [ ] **Step 1: Add the constructor**

Open `crates/ironmem/src/collab/session.rs`. After the existing `pub fn new(id: impl Into<String>) -> Self { … }` method (ends at the closing `}` around line 52), add:

```rust
    /// Construct a session pre-positioned at the v3 global-review stage.
    /// Used by the coding-review shortcut (`collab_start_code_review`) for
    /// orchestrators that already completed per-task coding via
    /// `subagent-driven-development`. The no-op `CodeReviewLocalPending`
    /// handshake is collapsed — `head_sha` is supplied here instead.
    pub fn new_global_review(
        id: impl Into<String>,
        base_sha: impl Into<String>,
        head_sha: impl Into<String>,
    ) -> Self {
        let head = head_sha.into();
        Self {
            id: id.into(),
            phase: Phase::CodeReviewFixGlobalPending,
            current_owner: "codex".to_string(),
            claude_draft_hash: None,
            codex_draft_hash: None,
            canonical_plan_hash: None,
            final_plan_hash: None,
            codex_review_verdict: None,
            review_round: 0,
            task_list: None,
            current_task_index: None,
            task_review_round: 0,
            global_review_round: 0,
            base_sha: Some(base_sha.into()),
            last_head_sha: Some(head),
            pr_url: None,
            coding_failure: None,
        }
    }
```

- [ ] **Step 2: Run the crate compile to verify no warnings**

Run: `cargo build -p ironmem`
Expected: successful build, no warnings about the new method.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
cargo clippy -p ironmem -- -D warnings
git add crates/ironmem/src/collab/session.rs
git commit -m "feat(collab): add CollabSession::new_global_review constructor"
```

---

## Task 2: Add `start_global_review_session` state-machine helper + unit tests

**Files:**
- Modify: `crates/ironmem/src/collab/state_machine/mod.rs`
- Modify: `crates/ironmem/src/collab/mod.rs`
- Test: `crates/ironmem/src/collab/state_machine/tests.rs`

- [ ] **Step 1: Write the failing tests**

Open `crates/ironmem/src/collab/state_machine/tests.rs`. Append:

```rust
#[test]
fn start_global_review_session_seeds_codex_owned_review_phase() {
    let session =
        start_global_review_session("s1", "basesha", "headsha").unwrap();
    assert_eq!(session.id, "s1");
    assert_eq!(session.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(session.current_owner, "codex");
    assert_eq!(session.base_sha.as_deref(), Some("basesha"));
    assert_eq!(session.last_head_sha.as_deref(), Some("headsha"));
    assert!(session.task_list.is_none());
    assert!(session.current_task_index.is_none());
    assert!(session.final_plan_hash.is_none());
    assert_eq!(session.review_round, 0);
}

#[test]
fn start_global_review_session_rejects_empty_base_sha() {
    let err = start_global_review_session("s1", "", "headsha").unwrap_err();
    assert!(matches!(err, CollabError::MissingBaseSha));
}

#[test]
fn start_global_review_session_rejects_empty_head_sha() {
    let err = start_global_review_session("s1", "basesha", "").unwrap_err();
    assert!(matches!(err, CollabError::MissingHeadSha));
}

#[test]
fn start_global_review_session_flows_into_final_review() {
    let session =
        start_global_review_session("s1", "basesha", "h0").unwrap();

    let after_codex = apply_event(
        &session,
        "codex",
        &CollabEvent::CodeReviewFixGlobal {
            head_sha: "h1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(after_codex.phase, Phase::CodeReviewFinalPending);
    assert_eq!(after_codex.current_owner, "claude");

    let after_claude = apply_event(
        &after_codex,
        "claude",
        &CollabEvent::FinalReview {
            head_sha: "h1".to_string(),
            pr_url: "https://github.com/acme/repo/pull/1".to_string(),
        },
    )
    .unwrap();
    assert_eq!(after_claude.phase, Phase::CodingComplete);
    assert_eq!(
        after_claude.pr_url.as_deref(),
        Some("https://github.com/acme/repo/pull/1")
    );
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p ironmem --lib collab::state_machine::tests::start_global_review_session`
Expected: FAIL with "cannot find function `start_global_review_session`" (or similar) — the helper and `MissingHeadSha` error variant don't exist yet.

- [ ] **Step 3: Add the `MissingHeadSha` error variant**

Open `crates/ironmem/src/collab/error.rs`. Find the `CollabError` enum. Add a new variant adjacent to the existing `MissingBaseSha`:

```rust
    #[error("head_sha is required but missing or empty")]
    MissingHeadSha,
```

(Use the exact attribute syntax already used in that file — `thiserror` `#[error("…")]` lines. If `MissingBaseSha` lives among other variants, slot `MissingHeadSha` next to it, preserving file ordering.)

- [ ] **Step 4: Add the helper function**

Open `crates/ironmem/src/collab/state_machine/mod.rs`. At the top of the file (after `use` statements, before `MAX_REVIEW_ROUNDS`), add:

```rust
/// Construct a fresh `CollabSession` positioned at the v3 global-review
/// stage, for the coding-review shortcut. Rejects empty SHAs so the
/// session never enters the review flow with unset drift-detection state.
pub fn start_global_review_session(
    id: &str,
    base_sha: &str,
    head_sha: &str,
) -> Result<CollabSession, CollabError> {
    if base_sha.is_empty() {
        return Err(CollabError::MissingBaseSha);
    }
    if head_sha.is_empty() {
        return Err(CollabError::MissingHeadSha);
    }
    Ok(CollabSession::new_global_review(id, base_sha, head_sha))
}
```

- [ ] **Step 5: Re-export from `collab/mod.rs` if the module has a re-export pattern**

Open `crates/ironmem/src/collab/mod.rs`. Find the existing re-exports (e.g., `pub use state_machine::apply_event;`). If there is such a line, add `start_global_review_session` alongside it:

```rust
pub use state_machine::{apply_event, start_global_review_session};
```

If `apply_event` is not re-exported from `mod.rs`, leave this step as a no-op — call sites will use `crate::collab::state_machine::start_global_review_session` directly.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p ironmem --lib collab::state_machine::tests::start_global_review_session`
Expected: 4 tests pass.

- [ ] **Step 7: Run the full collab test module**

Run: `cargo test -p ironmem --lib collab`
Expected: all existing collab tests still pass (no regressions).

- [ ] **Step 8: Commit**

```bash
cargo fmt --all
cargo clippy -p ironmem -- -D warnings
git add crates/ironmem/src/collab/error.rs \
        crates/ironmem/src/collab/state_machine/mod.rs \
        crates/ironmem/src/collab/state_machine/tests.rs \
        crates/ironmem/src/collab/mod.rs
git commit -m "feat(collab): add start_global_review_session state-machine entry"
```

---

## Task 3: Add `handle_collab_start_code_review` MCP handler

**Files:**
- Modify: `crates/ironmem/src/mcp/tools/collab_session.rs`

- [ ] **Step 1: Add the handler**

Open `crates/ironmem/src/mcp/tools/collab_session.rs`. After the existing `handle_collab_start` function (ends around line 143), add:

```rust
pub(super) fn handle_collab_start_code_review(
    app: &App,
    args: &Value,
) -> Result<Value, MemoryError> {
    let repo_path = require_str(args, "repo_path")?;
    let branch = require_str(args, "branch")?;
    let base_sha = require_str(args, "base_sha")?;
    let head_sha = require_str(args, "head_sha")?;
    let initiator = require_agent(require_str(args, "initiator")?)?;
    if initiator != "claude" {
        return Err(MemoryError::Validation(
            "collab_start_code_review requires initiator='claude'".to_string(),
        ));
    }
    let task_owned = args
        .get("task")
        .and_then(Value::as_str)
        .map(|value| sanitize::sanitize_content(value, MAX_COLLAB_CONTENT_CHARS))
        .transpose()?
        .map(ToString::to_string);
    let task = task_owned.as_deref();
    let session_id = uuid::Uuid::new_v4().to_string();

    let session = crate::collab::state_machine::start_global_review_session(
        &session_id,
        base_sha,
        head_sha,
    )
    .map_err(collab_error_to_memory_error)?;

    app.db.with_transaction(|tx| {
        crate::collab::queue::create_session(tx, &session_id, repo_path, branch, task)?;
        crate::collab::queue::save_session(tx, &session)?;
        crate::db::schema::Database::wal_log_tx(
            tx,
            "collab_start_code_review",
            &json!({
                "session_id": session_id,
                "repo_path": repo_path,
                "branch": branch,
                "initiator": initiator,
                "base_sha": base_sha,
                "head_sha": head_sha,
                "has_task": task.is_some(),
            }),
            Some(&json!({ "session_id": session_id })),
        )?;
        Ok(())
    })?;

    Ok(json!({ "session_id": session_id, "task": task }))
}
```

Note: `create_session` inserts a default row (phase=`PlanParallelDrafts`, owner=`claude`, etc.); `save_session` immediately overwrites those columns with the seeded global-review state. Both run in the same transaction so no partially-initialized row is ever visible.

- [ ] **Step 2: Verify the crate compiles**

Run: `cargo build -p ironmem`
Expected: successful build.

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
cargo clippy -p ironmem -- -D warnings
git add crates/ironmem/src/mcp/tools/collab_session.rs
git commit -m "feat(mcp): handle_collab_start_code_review session handler"
```

---

## Task 4: Register the tool in the MCP tool registry

**Files:**
- Modify: `crates/ironmem/src/mcp/tools/mod.rs`

- [ ] **Step 1: Import the new handler**

Open `crates/ironmem/src/mcp/tools/mod.rs`. Find the import on line 20:

```rust
    handle_collab_send, handle_collab_start, handle_collab_status, handle_collab_wait_my_turn,
```

Replace with:

```rust
    handle_collab_send, handle_collab_start, handle_collab_start_code_review,
    handle_collab_status, handle_collab_wait_my_turn,
```

- [ ] **Step 2: Add the tool schema entry**

Find the `collab_start` schema block (starts around line 201 with `"name": "collab_start"`). Immediately after its closing `}),` add a new schema entry:

```rust
        json!({
            "name": "collab_start_code_review",
            "description": "Shortcut entry point: create a collab session positioned directly at the v3 global-review stage (CodeReviewFixGlobalPending, owner 'codex'). Skips v1 planning and v3 per-task coding — use when the branch was already coded via superpowers:subagent-driven-development and only needs the end-of-branch Codex review + Claude PR-open. Requires initiator='claude', base_sha (pre-subagent-driven-dev HEAD), and head_sha (current branch HEAD).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "repo_path": { "type": "string" },
                    "branch": { "type": "string" },
                    "base_sha": { "type": "string" },
                    "head_sha": { "type": "string" },
                    "initiator": { "type": "string", "enum": ["claude"] },
                    "task": { "type": "string" }
                },
                "required": ["repo_path", "branch", "base_sha", "head_sha", "initiator"]
            }
        }),
```

- [ ] **Step 3: Wire the dispatch arm**

Find line `"collab_start" => handle_collab_start(app, args),` (around line 375). Immediately after it, add:

```rust
        "collab_start_code_review" => handle_collab_start_code_review(app, args),
```

- [ ] **Step 4: Add the name to `tool_known`**

Find the `tool_known` fn (around line 393) with its `matches!(name, …)` literal list. Add `"collab_start_code_review"` adjacent to `"collab_start"`:

```rust
            | "collab_start"
            | "collab_start_code_review"
            | "collab_send"
```

- [ ] **Step 5: Add the name to the write-mode gate list**

Find the `tool_allowed_in_mode` fn (around line 426). In the `!matches!(name, …)` list of write-only tools, add `"collab_start_code_review"`:

```rust
            "add_drawer"
                | "delete_drawer"
                | "kg_add"
                | "kg_invalidate"
                | "diary_write"
                | "collab_start"
                | "collab_start_code_review"
                | "collab_send"
                | "collab_ack"
                | "collab_approve"
                | "collab_register_caps"
                | "collab_end"
```

- [ ] **Step 6: Verify the crate compiles**

Run: `cargo build -p ironmem`
Expected: successful build.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
cargo clippy -p ironmem -- -D warnings
git add crates/ironmem/src/mcp/tools/mod.rs
git commit -m "feat(mcp): register collab_start_code_review tool"
```

---

## Task 5: End-to-end MCP protocol test — happy path

**Files:**
- Test: `crates/ironmem/tests/mcp_protocol.rs`

- [ ] **Step 1: Write the failing test**

Open `crates/ironmem/tests/mcp_protocol.rs`. At the end of the file (after the last `#[test]`), append:

```rust
#[test]
fn collab_start_code_review_lands_in_codex_review_phase() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/xyz",
            "base_sha": "basesha",
            "head_sha": "headsha",
            "initiator": "claude",
            "task": "add landing page"
        }),
    );
    assert_eq!(started["task"], "add landing page");
    let session_id = started["session_id"].as_str().unwrap();

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodeReviewFixGlobalPending");
    assert_eq!(status["current_owner"], "codex");
    assert_eq!(status["base_sha"], "basesha");
    assert_eq!(status["last_head_sha"], "headsha");
    assert!(status["task_list"].is_null());
    assert!(status["final_plan_hash"].is_null());
}

#[test]
fn collab_start_code_review_flows_to_coding_complete() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/xyz",
            "base_sha": "basesha",
            "head_sha": "h0",
            "initiator": "claude",
            "task": "add landing page"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    // Codex reviews + pushes fixes, advancing to CodeReviewFinalPending.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "review_fix_global",
            "content": json!({ "head_sha": "h1" }).to_string()
        }),
    );
    let mid = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(mid["phase"], "CodeReviewFinalPending");
    assert_eq!(mid["current_owner"], "claude");

    // Claude opens PR and finalizes.
    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "claude",
            "topic": "final_review",
            "content": json!({
                "head_sha": "h1",
                "pr_url": "https://github.com/acme/repo/pull/1"
            }).to_string()
        }),
    );
    let done = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(done["phase"], "CodingComplete");
    assert_eq!(done["pr_url"], "https://github.com/acme/repo/pull/1");
}
```

- [ ] **Step 2: Run the tests to verify the first fails (should already pass if Tasks 1-4 are done, but confirm)**

Run: `cargo test -p ironmem --test mcp_protocol collab_start_code_review`
Expected: both tests PASS. (If a test fails here, treat it as a regression in Tasks 1-4 and fix root cause before proceeding.)

- [ ] **Step 3: Commit**

```bash
cargo fmt --all
cargo clippy -p ironmem --tests -- -D warnings
git add crates/ironmem/tests/mcp_protocol.rs
git commit -m "test(collab): happy-path MCP tests for collab_start_code_review"
```

---

## Task 6: End-to-end MCP protocol test — rejections and failure path

**Files:**
- Test: `crates/ironmem/tests/mcp_protocol.rs`

- [ ] **Step 1: Write the failing tests**

Append to `crates/ironmem/tests/mcp_protocol.rs`:

```rust
#[test]
fn collab_start_code_review_rejects_codex_initiator() {
    let app = App::open_for_test().unwrap();
    let result = crate::call_tool_raw(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/xyz",
            "base_sha": "basesha",
            "head_sha": "headsha",
            "initiator": "codex"
        }),
    );
    assert!(
        result.is_err(),
        "codex-initiated shortcut session must be rejected"
    );
}

#[test]
fn collab_start_code_review_rejects_empty_base_sha() {
    let app = App::open_for_test().unwrap();
    let result = crate::call_tool_raw(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/xyz",
            "base_sha": "",
            "head_sha": "headsha",
            "initiator": "claude"
        }),
    );
    assert!(result.is_err());
}

#[test]
fn collab_start_code_review_collab_end_rejected_during_review() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/xyz",
            "base_sha": "basesha",
            "head_sha": "h0",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    let end_result = crate::call_tool_raw(
        &app,
        "collab_end",
        json!({ "session_id": session_id, "agent": "claude" }),
    );
    assert!(
        end_result.is_err(),
        "collab_end must be rejected during CodeReviewFixGlobalPending"
    );
}

#[test]
fn collab_start_code_review_failure_report_reaches_coding_failed() {
    let app = App::open_for_test().unwrap();
    let started = call_tool(
        &app,
        "collab_start_code_review",
        json!({
            "repo_path": "/repo",
            "branch": "feat/xyz",
            "base_sha": "basesha",
            "head_sha": "h0",
            "initiator": "claude"
        }),
    );
    let session_id = started["session_id"].as_str().unwrap();

    call_tool(
        &app,
        "collab_send",
        json!({
            "session_id": session_id,
            "sender": "codex",
            "topic": "failure_report",
            "content": json!({ "coding_failure": "local tests broken" }).to_string()
        }),
    );

    let status = call_tool(&app, "collab_status", json!({ "session_id": session_id }));
    assert_eq!(status["phase"], "CodingFailed");
    assert_eq!(status["coding_failure"], "local tests broken");
}
```

Note: `call_tool_raw` is the `Result`-returning variant used for negative tests. If the test file already has a helper by a different name (e.g., `try_call_tool`), substitute the existing name. If no such helper exists, either add one at the top of the test file, or wrap the `call_tool` panic-catching at call sites. Before Step 2, inspect the file and use/define the helper that matches existing conventions.

- [ ] **Step 2: Verify the test file's negative-test helper convention**

Run: `grep -n "call_tool_raw\|try_call_tool\|tool.*Result" crates/ironmem/tests/mcp_protocol.rs`
Expected: Reveals whether a raw/result-returning helper exists. If it does, rename the four `call_tool_raw` references above to match. If it does not, add the following helper near the top of the file (after the existing `call_tool` helper):

```rust
fn call_tool_raw(app: &App, name: &str, args: Value) -> Result<Value, ironmem::MemoryError> {
    ironmem::mcp::tools::call_tool(app, name, &args)
}
```

(Adjust the `MemoryError` import path to match the file's existing `use` statements.)

- [ ] **Step 3: Run the new tests**

Run: `cargo test -p ironmem --test mcp_protocol collab_start_code_review`
Expected: all 6 `collab_start_code_review_*` tests PASS.

- [ ] **Step 4: Run the full test suite to check for regressions**

Run: `cargo test -p ironmem`
Expected: all tests pass, zero regressions.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
cargo clippy -p ironmem --tests -- -D warnings
git add crates/ironmem/tests/mcp_protocol.rs
git commit -m "test(collab): rejection + failure-path tests for collab_start_code_review"
```

---

## Task 7: Document the shortcut in `docs/COLLAB.md`

**Files:**
- Modify: `docs/COLLAB.md`

- [ ] **Step 1: Add the shortcut subsection**

Open `docs/COLLAB.md`. Find the "Failure + terminal" subsection inside the "v3 Coding Phase Model" section (starts with `### Failure + terminal` and ends around line 181 before "## Blind-Draft Invariant"). Immediately after that subsection and before "## Blind-Draft Invariant", insert:

```markdown
### Shortcut: post-subagent coding review

Orchestrators that already completed per-task coding outside the collab
protocol (for example, via `superpowers:subagent-driven-development`) can
skip v1 planning and v3 per-task phases entirely by calling
`collab_start_code_review`. The session starts directly in
`CodeReviewFixGlobalPending` with `current_owner = codex` — the no-op
Claude `ReviewLocal` handshake is collapsed because `head_sha` is supplied
at session creation.

From creation, the remaining two turns run unchanged:

| Phase | Owner | Event |
|---|---|---|
| `CodeReviewFixGlobalPending` | `codex` | `CodeReviewFixGlobal{head_sha}` |
| `CodeReviewFinalPending` | `claude` | `FinalReview{head_sha, pr_url}` → `CodingComplete` |

Constraints:

- `initiator` must be `claude`; Codex cannot start a shortcut session.
- `base_sha` and `head_sha` are required and must be non-empty.
- No `task_list`, `plan_hash`, or `final_plan_hash` fields are populated on
  the session — the v1 planning and per-task coding histories do not exist
  for shortcut sessions.
- `collab_end` is rejected during both `CodeReviewFixGlobalPending` and
  `CodeReviewFinalPending`, same rule as any coding-active phase. Escape
  hatch is `failure_report` → `CodingFailed`.
- Drift detection uses the supplied `base_sha` / `head_sha` pair exactly
  like the full-flow global-review stage; no special casing.
```

- [ ] **Step 2: Add the tool to the "MCP Tools" section**

Find the `## MCP Tools` section (around line 192) and its subsection `### collab_start`. Immediately after the `collab_start` subsection, add:

```markdown
### `collab_start_code_review`

Shortcut entry. Creates a session positioned at `CodeReviewFixGlobalPending`,
owner `codex`. See the "Shortcut: post-subagent coding review" subsection
above for constraints.

```json
{
  "repo_path": "/path/to/repo",
  "branch": "feat/landing-page",
  "base_sha": "abc123",
  "head_sha": "def456",
  "initiator": "claude",
  "task": "add landing page"
}
```

Returns `{ session_id, task }`. The `task` is stored on the session and is
readable via `collab_status`.
```

- [ ] **Step 3: Verify the doc renders cleanly**

Run: `grep -n "collab_start_code_review\|Shortcut: post-subagent" docs/COLLAB.md`
Expected: three matches — the heading, the MCP Tools subsection, and one mention in the narrative.

- [ ] **Step 4: Commit**

```bash
git add docs/COLLAB.md
git commit -m "docs(collab): document collab_start_code_review shortcut"
```

---

## Task 8: Add `/collab review` subcommand to the Claude slash command

**Files:**
- Modify: `.claude-plugin/commands/collab.md`

- [ ] **Step 1: Update the frontmatter description and argument hint**

Open `.claude-plugin/commands/collab.md`. Replace the frontmatter (lines 1-4) with:

```markdown
---
description: Start or join an IronRace bounded planning session with Codex, auto-flowing into v3 coding if enabled. Covers v1 planning, v3 per-task linear → global review → PR handoff, and the post-subagent review shortcut. Usage — /collab start <task>  |  /collab join <session_id>  |  /collab review <short-topic>
argument-hint: start <task> | join <session_id> | review <short-topic>
---
```

- [ ] **Step 2: Add the `review` subcommand section**

Find the `## \`join <session_id>\`` section header (around line 54). Insert the following new section immediately before it:

```markdown
## `review <short-topic>`

Shortcut entry for post-subagent-driven-development flows: skip v1 planning
and v3 per-task coding, drop straight into the v3 global-review stage with
Codex as the reviewer on the already-committed branch. Everything except
the short topic is inferred — never ask the user for paths, branches, or
SHAs.

1. Resolve defaults:
   - `repo_path` ← output of `git rev-parse --show-toplevel`.
   - `branch` ← output of `git branch --show-current`. If the result is
     empty (detached HEAD) or equals `main`/`master`/`trunk`, abort with
     an error message explaining the shortcut requires a feature branch.
   - `head_sha` ← output of `git rev-parse HEAD`.
   - `base_sha` ← output of `git merge-base origin/main HEAD` (fall back
     to `origin/master` if the first command fails, then `origin/trunk`).
     Abort if all three fail with a message asking the user to set an
     upstream.
   - `initiator` ← `"claude"`.
   - `task` ← the remainder of `$ARGUMENTS` after the word `review`.
2. Call `mcp__ironmem__collab_start_code_review` with
   `{repo_path, branch, base_sha, head_sha, initiator, task}`.
3. Report the session id back as a single line:

   ```
   Collab review session started: <session_id>
   ```

4. **Do not enter Plan Mode and do not draft anything.** The shortcut
   positions the session at `CodeReviewFixGlobalPending` — the next action
   is Codex's review turn, driven inline via `mcp__codex__codex` under
   the existing "Codex handoff — synchronous MCP invocation" rules.
5. Enter the v3 dispatch loop at phase `CodeReviewFixGlobalPending` (see
   the "v3 Dispatch Loop" table). The loop should handle the two
   remaining turns (`review_fix_global` from Codex, then `final_review`
   from Claude) and terminate at `CodingComplete`.
```

- [ ] **Step 3: Update the v3 Dispatch Loop table to document the shortcut entry (if the table's current text says the loop starts from per-task phases)**

Find the `## v3 Dispatch Loop` section (around line 155). At the end of that section (before `### Anti-puppeteering rules (v3)`), add:

```markdown
**Shortcut entry:** `/collab review` starts the loop at phase
`CodeReviewFixGlobalPending` with `current_owner == "codex"`. No per-task
phases are ever traversed. The only two remaining turns are Codex's
`review_fix_global` and Claude's `final_review`. All anti-puppeteering
rules below apply unchanged.
```

- [ ] **Step 4: Update the "Unknown subcommand" section**

Find the `## Unknown subcommand` section (around line 312). Update any message that lists valid subcommands to include `review <short-topic>`:

```markdown
If `$ARGUMENTS` does not start with `start`, `join`, or `review`, respond
with a one-liner explaining valid usage: `/collab start <task>`, `/collab
join <session_id>`, or `/collab review <short-topic>`.
```

(Match the existing phrasing — the goal is just to add `review` as a third option.)

- [ ] **Step 5: Verify the file**

Run: `grep -n "review <short-topic>\|collab_start_code_review" .claude-plugin/commands/collab.md`
Expected: at least 4 matches — frontmatter description, argument-hint, new section header, one MCP tool reference.

- [ ] **Step 6: Commit**

```bash
git add .claude-plugin/commands/collab.md
git commit -m "feat(collab): add /collab review slash subcommand"
```

---

## Task 9: Mirror the `/collab review` flow in the Codex prompt

**Files:**
- Modify: `.codex-plugin/prompts/collab.md`

- [ ] **Step 1: Inspect the Codex prompt structure**

Read the file to identify the equivalent section boundaries on the Codex side:

Run: `grep -n "^## \|^### " .codex-plugin/prompts/collab.md`

The Codex mirror typically documents how Codex responds to a running
session (not how it initiates one), so the Codex-side change is narrower:
add awareness that a session may land directly in
`CodeReviewFixGlobalPending` without any prior planning or per-task
history.

- [ ] **Step 2: Add a "Shortcut awareness" note**

Open `.codex-plugin/prompts/collab.md`. Find the v3 dispatch / coding
section (analogous to Claude's "v3 Dispatch Loop"). Add (placement per
the file's existing section structure):

```markdown
### Shortcut-entered sessions (post-subagent review)

A session may be created via `collab_start_code_review` and land directly
at `CodeReviewFixGlobalPending` with `current_owner == "codex"`. When
Codex joins such a session:

- `task_list`, `final_plan_hash`, and planning-phase fields will all be
  null in `collab_status`.
- `base_sha` and `last_head_sha` will be set — use them for branch-drift
  detection exactly as in a full-flow global review.
- Codex's next turn is `review_fix_global`; after that, Claude's
  `final_review` closes out the session. No earlier phases are ever
  reachable from a shortcut session.

All existing v3 anti-puppeteering rules apply unchanged.
```

- [ ] **Step 3: Verify the file**

Run: `grep -n "Shortcut-entered\|collab_start_code_review" .codex-plugin/prompts/collab.md`
Expected: at least 2 matches.

- [ ] **Step 4: Commit**

```bash
git add .codex-plugin/prompts/collab.md
git commit -m "docs(collab): mirror shortcut awareness in Codex prompt"
```

---

## Task 10: Final verification

**Files:** None (verification only).

- [ ] **Step 1: Run the full test suite**

Run: `cargo test -p ironmem`
Expected: all tests pass.

- [ ] **Step 2: Run format and clippy on the whole workspace**

Run: `cargo fmt --all --check && cargo clippy --all-targets -- -D warnings`
Expected: no formatting diffs, no clippy warnings.

- [ ] **Step 3: Smoke-test the new tool schema is discoverable**

Run: `cargo test -p ironmem --test mcp_protocol -- --list | grep collab_start_code_review`
Expected: 6 test names listed (the happy-path + rejection tests added in Tasks 5-6).

- [ ] **Step 4: Re-read `docs/COLLAB.md` and slash command files for consistency**

Visually check that:
- Phase name `CodeReviewFixGlobalPending` is spelled consistently across all three files.
- Tool name `collab_start_code_review` matches exactly.
- The `/collab review` subcommand example exists in both the Claude slash command and the Codex prompt (the Codex prompt mirrors Codex's response behavior, not initiation).

- [ ] **Step 5: Final commit (if any fixes were needed during verification)**

If Steps 1-4 surfaced issues, fix them and commit with message: `chore(collab): post-verification fixes`. If everything was clean, skip.

---

## Self-Review Checklist

Against the spec (`docs/superpowers/specs/2026-04-22-collab-coding-review-shortcut-design.md`):

- **Spec §2 (tool inputs):** Task 3 handler extracts `repo_path`, `branch`, `base_sha`, `head_sha`, `initiator`, optional `task`; rejects `initiator != "claude"`. ✓
- **Spec §2 (session state at creation):** Task 1 constructor sets `phase=CodeReviewFixGlobalPending`, `current_owner=codex`, `base_sha`, `last_head_sha`, leaves all planning + per-task fields null. ✓
- **Spec §2 (collapsed handshake):** Task 1 seeds `CodeReviewFixGlobalPending` directly; no `CodeReviewLocalPending`/`ReviewLocal` in the flow. ✓
- **Spec §3 (flow unchanged from creation onward):** Tasks 5-6 verify the existing `CodeReviewFixGlobal` → `FinalReview` → `CodingComplete` transitions run without modification. ✓
- **Spec §4 (drift detection reuses existing machinery):** No state-machine changes; existing transition handlers in `state_machine/mod.rs:175-187` run unchanged. ✓
- **Spec §4 (terminal set):** Task 6 verifies `collab_end` is rejected and `failure_report` reaches `CodingFailed`. ✓
- **Spec §4 (collab_end rejection):** Task 6 test covers this. ✓
- **Spec §4 (capabilities registration unchanged):** No test needed — the capability machinery is orthogonal and untouched. Noted in self-review for completeness.
- **Spec §4 (re-entry):** Sessions terminate at `CodingComplete`/`CodingFailed` just like full-flow sessions; no special "re-entry" behavior to test.
- **Spec §4 (initiator=codex rejected):** Task 6 test covers this. ✓
- **Spec §5 (slash command auto-fill):** Task 8 specifies `git rev-parse`, `git branch --show-current`, `git merge-base` resolution. ✓
- **Spec §6 (implementation touchpoints):** All six touchpoints mapped to Tasks 1-9. ✓
- **Spec §7 (non-breaking):** No existing tool signatures, phase names, event names, or DB columns change. ✓

Placeholder scan: no "TBD", "TODO", or "similar to" references in any task. All code blocks contain complete executable content. All file paths are absolute-from-repo-root.

Type consistency: `handle_collab_start_code_review` signature matches the existing handler pattern (`fn(&App, &Value) -> Result<Value, MemoryError>`); `start_global_review_session` signature matches what Task 3 calls (`(&str, &str, &str) -> Result<CollabSession, CollabError>`); `CollabSession::new_global_review` signature matches what Task 2's helper calls.
