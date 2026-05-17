# Collab v3 Phase Reorder (Codex First) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Reorder collab v3 phases so Codex `review_fix_global` runs BEFORE Claude `/ultrareview-local` audit, giving Codex first read of the raw post-implementation diff on every PR.

**Architecture:** The reorder is localized to four `match` arms in `state_machine/mod.rs:172-197` plus one shortcut-ancestry gate extension in `collab_session.rs:303-312`. The bulk of the work is updating tests, three protocol docs/prompts, and Rust source comments to reflect the new order. No event names change; no protocol-version migration is added; deployment is forward-only with a documented drain requirement.

**Tech Stack:** Rust (cargo workspace), markdown (docs/protocol prompts), git/gh, ironmem MCP collab tools.

**Codex review status:** Round 1 `request_changes` (5 notes addressed) → round 2 `approve_with_minor_edits` (5 minor edits applied). Branch: `feat/collab-v3-reorder-codex-first` off PR #55 head `a6c1889`. Session: `4e305afa-d747-47e0-8744-dd9a5b6068f6`.

---

## Task 1: Preflight grep — establish current transition surface

**Files:**
- Read-only — no edits.

**Acceptance:**
- Three greps execute cleanly and the results are inspected before any subsequent task starts.
- Confirms that the state-machine map (mod.rs:172-197, collab_session.rs:303-312) is complete; any newly-discovered sites get added to the relevant later task before that task starts.

- [ ] **Step 1: Phase variant references**

Run:
```bash
rg -n 'CodeReviewLocalPending|CodeReviewFixGlobalPending|CodeReviewFinalPending' \
   crates/ironmem/src/collab/ docs/COLLAB.md .claude-plugin/ .codex-plugin/
```
Expected: matches grouped by file. Verify the four state-machine arms (mod.rs:172-197) and the shortcut gate (collab_session.rs:303-312) are present.

- [ ] **Step 2: Topic + event references**

Run:
```bash
rg -n 'review_local|review_fix_global|final_review|implementation_done|start_code_review' \
   crates/ironmem/src/collab/ docs/COLLAB.md .claude-plugin/ .codex-plugin/
```
Expected: surfaces every doc/prompt site that mentions a v3 topic.

- [ ] **Step 3: Owner-assignment references**

Run:
```bash
rg -n 'current_owner|Agent::Codex|Agent::Claude' crates/ironmem/src/collab/
```
Expected: confirms owner assignments are inline in the four match arms.

- [ ] **Step 4: Decide if scope expands**

If steps 1-3 surface a file not listed in Tasks 2-10, add it to the relevant task's `Files:` block before starting that task. Do not start Task 2 until preflight is reviewed.

**No commit in this task** (verification only).

---

## Task 2: TDD — write new failing tests for v3 sequence + shortcut audit flow + v1 cap regression

**Files:**
- Modify: `crates/ironmem/src/collab/state_machine/tests.rs` — add three new tests near the existing `test_global_review_linear_flow_ends_in_coding_complete` (around line 454).
- Modify: `crates/ironmem/tests/mcp_protocol.rs` — add new shortcut-audit-flow integration test near the existing shortcut-test cluster.

**Acceptance:**
- New tests compile.
- Running them shows the v3 sequence test FAILS with phase-mismatch (state machine still sends `ImplementationDone` to `CodeReviewLocalPending`).
- The v1 force-finalize regression test PASSES (it documents existing behavior; we want a regression assert, not a new feature).

- [ ] **Step 1: Add `test_v3_phase_sequence_is_global_then_local`**

In `crates/ironmem/src/collab/state_machine/tests.rs`, after the existing `test_global_review_linear_flow_ends_in_coding_complete` (around line 517), add:

```rust
#[test]
fn test_v3_phase_sequence_is_global_then_local() {
    let mut s = new_session_at_code_implement_pending();
    // CodeImplementPending -> CodeReviewFixGlobalPending (Codex)
    apply_event(&mut s, CollabEvent::ImplementationDone { head_sha: "h1".into() })
        .expect("implementation_done");
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, Agent::Codex);

    // CodeReviewFixGlobalPending -> CodeReviewLocalPending (Claude)
    apply_event(&mut s, CollabEvent::CodeReviewFixGlobal { head_sha: "h2".into() })
        .expect("review_fix_global");
    assert_eq!(s.phase, Phase::CodeReviewLocalPending);
    assert_eq!(s.current_owner, Agent::Claude);

    // CodeReviewLocalPending -> CodeReviewFinalPending (Claude)
    apply_event(&mut s, CollabEvent::ReviewLocal { head_sha: "h3".into() })
        .expect("review_local");
    assert_eq!(s.phase, Phase::CodeReviewFinalPending);
    assert_eq!(s.current_owner, Agent::Claude);

    // CodeReviewFinalPending -> CodingComplete
    apply_event(&mut s, CollabEvent::FinalReview {
        head_sha: "h4".into(),
        pr_url: "https://example/pr".into(),
    })
    .expect("final_review");
    assert_eq!(s.phase, Phase::CodingComplete);
}
```

Reuse `new_session_at_code_implement_pending`/`apply_event` if they exist in the test module; otherwise mirror the helper pattern used by `finish_through_global_review()` (lines 111-145).

- [ ] **Step 2: Add `test_v1_force_finalize_still_works_at_max_rounds`**

In the same test file, add:

```rust
#[test]
fn test_v1_force_finalize_still_works_at_max_rounds() {
    // Walk v1: draft, draft, canonical, request_changes, canonical, request_changes
    // Server should force-finalize to PlanClaudeFinalizePending on the 2nd request_changes.
    let mut s = new_session_at_plan_parallel_drafts();
    apply_event(&mut s, CollabEvent::Draft { sender: Agent::Claude, hash: "c".into() }).unwrap();
    apply_event(&mut s, CollabEvent::Draft { sender: Agent::Codex,  hash: "x".into() }).unwrap();
    apply_event(&mut s, CollabEvent::Canonical { hash: "k1".into() }).unwrap();
    apply_event(&mut s, CollabEvent::Review {
        verdict: ReviewVerdict::RequestChanges, hash: "k1".into(),
    }).unwrap();
    assert_eq!(s.phase, Phase::PlanSynthesisPending);
    apply_event(&mut s, CollabEvent::Canonical { hash: "k2".into() }).unwrap();
    apply_event(&mut s, CollabEvent::Review {
        verdict: ReviewVerdict::RequestChanges, hash: "k2".into(),
    }).unwrap();
    // Force-finalize trips here (review_round == MAX_REVIEW_ROUNDS).
    assert_eq!(s.phase, Phase::PlanClaudeFinalizePending);
    assert_eq!(s.review_round, 2);
}
```

If the existing test suite already covers this assertion (e.g., `state_machine/tests.rs:203`), add an explicit pointer comment instead of duplicating; the goal is regression coverage explicit enough that a future reorder would not silently bypass it.

- [ ] **Step 3: Add `test_shortcut_review_flows_through_audit` integration test**

In `crates/ironmem/tests/mcp_protocol.rs`, near the existing shortcut test (look for `collab_start_code_review` in tests around lines 1063-1140), add:

```rust
#[tokio::test]
async fn test_shortcut_review_flows_through_audit() {
    let mut h = TestHarness::new().await;
    let sid = h
        .collab_start_code_review("/repo", "feat/x", "base", "head1", "claude", "shortcut audit")
        .await
        .expect("start_code_review");
    // Shortcut seeds at CodeReviewFixGlobalPending / Codex.
    assert_eq!(h.collab_status(&sid).await.phase, "CodeReviewFixGlobalPending");
    assert_eq!(h.collab_status(&sid).await.current_owner, "codex");

    // Codex pushes review_fix_global -> CodeReviewLocalPending / Claude.
    h.collab_send(&sid, "codex", "review_fix_global", r#"{"head_sha":"head2"}"#).await.unwrap();
    assert_eq!(h.collab_status(&sid).await.phase, "CodeReviewLocalPending");
    assert_eq!(h.collab_status(&sid).await.current_owner, "claude");

    // Claude audits -> CodeReviewFinalPending / Claude.
    h.collab_send(&sid, "claude", "review_local", r#"{"head_sha":"head3"}"#).await.unwrap();
    assert_eq!(h.collab_status(&sid).await.phase, "CodeReviewFinalPending");

    // Claude PRs -> CodingComplete.
    h.collab_send(
        &sid, "claude", "final_review",
        r#"{"head_sha":"head4","pr_url":"https://example/pr"}"#,
    ).await.unwrap();
    assert_eq!(h.collab_status(&sid).await.phase, "CodingComplete");
}
```

Use the actual `TestHarness` helper if it exists; otherwise pattern-match the existing shortcut test's setup at `tests/mcp_protocol.rs:1063`.

- [ ] **Step 4: Run the new tests — expect failures**

Run:
```bash
cargo test --package ironmem --lib test_v3_phase_sequence_is_global_then_local -- --exact
cargo test --package ironmem --lib test_v1_force_finalize_still_works_at_max_rounds -- --exact
cargo test --package ironmem --test mcp_protocol test_shortcut_review_flows_through_audit -- --exact
```

Expected:
- `test_v3_phase_sequence_is_global_then_local` → FAIL (assertion: `Phase::CodeReviewLocalPending != Phase::CodeReviewFixGlobalPending` at the first transition).
- `test_v1_force_finalize_still_works_at_max_rounds` → PASS (regression baseline).
- `test_shortcut_review_flows_through_audit` → FAIL (assertion: after `review_fix_global` phase is `CodeReviewFinalPending`, not `CodeReviewLocalPending`).

- [ ] **Step 5: Commit**

```bash
git add crates/ironmem/src/collab/state_machine/tests.rs crates/ironmem/tests/mcp_protocol.rs
git commit -m "test(collab): RED — failing tests for v3 reorder + v1 cap regression"
```

---

## Task 3: Rewire state machine — four match arms

**Files:**
- Modify: `crates/ironmem/src/collab/state_machine/mod.rs:172-197`.

**Acceptance:**
- `test_v3_phase_sequence_is_global_then_local` PASSES.
- `test_shortcut_review_flows_through_audit` PASSES.
- Existing `test_implementation_done_jumps_to_local_review` FAILS (next task renames + rewires it).

- [ ] **Step 1: Read the current arms**

Read `crates/ironmem/src/collab/state_machine/mod.rs:172-197`.

Confirm the four current arms:

```rust
// Today:
(Phase::CodeImplementPending,        Event::ImplementationDone   { .. }) => (Phase::CodeReviewLocalPending,     Agent::Claude),
(Phase::CodeReviewLocalPending,      Event::ReviewLocal          { .. }) => (Phase::CodeReviewFixGlobalPending, Agent::Codex),
(Phase::CodeReviewFixGlobalPending,  Event::CodeReviewFixGlobal  { .. }) => (Phase::CodeReviewFinalPending,     Agent::Claude),
(Phase::CodeReviewFinalPending,      Event::FinalReview          { .. }) => (Phase::CodingComplete,             Agent::Claude),
```

- [ ] **Step 2: Rewrite to new order**

Replace the four arms with:

```rust
// New: Codex first (review_fix_global) -> Claude audit (review_local) -> Claude PR (final_review)
(Phase::CodeImplementPending,        Event::ImplementationDone   { .. }) => (Phase::CodeReviewFixGlobalPending, Agent::Codex),
(Phase::CodeReviewFixGlobalPending,  Event::CodeReviewFixGlobal  { .. }) => (Phase::CodeReviewLocalPending,     Agent::Claude),
(Phase::CodeReviewLocalPending,      Event::ReviewLocal          { .. }) => (Phase::CodeReviewFinalPending,     Agent::Claude),
(Phase::CodeReviewFinalPending,      Event::FinalReview          { .. }) => (Phase::CodingComplete,             Agent::Claude),
```

Preserve any `require_actor(...)` / error-message wiring at each arm — only the destination phase + owner change.

- [ ] **Step 3: Run the v3 sequence test**

Run:
```bash
cargo test --package ironmem --lib test_v3_phase_sequence_is_global_then_local -- --exact
cargo test --package ironmem --test mcp_protocol test_shortcut_review_flows_through_audit -- --exact
```
Expected: both PASS.

- [ ] **Step 4: Verify existing test fails — expected**

```bash
cargo test --package ironmem --lib test_implementation_done_jumps_to_local_review -- --exact 2>&1 | tail
```
Expected: FAIL. The next task renames + rewires this test.

- [ ] **Step 5: Commit**

```bash
git add crates/ironmem/src/collab/state_machine/mod.rs
git commit -m "feat(collab): reorder v3 — Codex review_fix_global precedes Claude review_local"
```

---

## Task 4: Update existing state machine + mcp_protocol tests

**Files:**
- Modify: `crates/ironmem/src/collab/state_machine/tests.rs:111-145` — `finish_through_global_review()` helper.
- Modify: `crates/ironmem/src/collab/state_machine/tests.rs:336-352` — `test_implementation_done_jumps_to_local_review`.
- Modify: `crates/ironmem/src/collab/state_machine/tests.rs:454-517` — `test_global_review_linear_flow_ends_in_coding_complete`.
- Modify: `crates/ironmem/tests/mcp_protocol.rs:1063-1140` — full v3 integration walk + shortcut-flow assertions.

**Acceptance:**
- All existing tests pass.
- `test_implementation_done_jumps_to_local_review` is renamed to `test_implementation_done_jumps_to_global_review` with rewired assertions.

- [ ] **Step 1: Rewire `finish_through_global_review()` helper (tests.rs:111-145)**

Update the helper to drive the new sequence. Walk:
1. `ImplementationDone` → expect `CodeReviewFixGlobalPending` / Codex.
2. `CodeReviewFixGlobal` → expect `CodeReviewLocalPending` / Claude.
3. `ReviewLocal` → expect `CodeReviewFinalPending` / Claude.
4. `FinalReview` → expect `CodingComplete`.

- [ ] **Step 2: Rename + rewire `test_implementation_done_jumps_to_local_review`**

Read current at tests.rs:336-352. Rename to `test_implementation_done_jumps_to_global_review`. Update the assertion:

```rust
#[test]
fn test_implementation_done_jumps_to_global_review() {
    let mut s = new_session_at_code_implement_pending();
    apply_event(&mut s, CollabEvent::ImplementationDone { head_sha: "h1".into() })
        .expect("implementation_done");
    assert_eq!(s.phase, Phase::CodeReviewFixGlobalPending);
    assert_eq!(s.current_owner, Agent::Codex);
}
```

- [ ] **Step 3: Rewire `test_global_review_linear_flow_ends_in_coding_complete` (lines 454-517)**

Update assertions at lines 467, 478, 490, 503 (per state-machine map). New sequence:
- Line ~467 (after `ImplementationDone`): assert `CodeReviewFixGlobalPending` / `Agent::Codex`.
- Line ~478 (after `CodeReviewFixGlobal`): assert `CodeReviewLocalPending` / `Agent::Claude`.
- Line ~490 (after `ReviewLocal`): assert `CodeReviewFinalPending` / `Agent::Claude`.
- Line ~503 (after `FinalReview`): assert `CodingComplete`.

- [ ] **Step 4: Rewire `tests/mcp_protocol.rs:1063-1140`**

Walk the integration test step-by-step:
- After `do_implementation_done(...)`: replace `assert_eq!(..., "CodeReviewLocalPending")` with `assert_eq!(..., "CodeReviewFixGlobalPending")`.
- After Codex's `collab_send(topic="review_fix_global", ...)`: replace `CodeReviewFinalPending` with `CodeReviewLocalPending`.
- After Claude's `collab_send(topic="review_local", ...)`: replace `CodeReviewFixGlobalPending` with `CodeReviewFinalPending`.
- After Claude's `collab_send(topic="final_review", ...)`: assertion remains `CodingComplete`.

- [ ] **Step 5: Run the full test suite**

```bash
cargo test --workspace
```
Expected: all tests pass (new and existing).

- [ ] **Step 6: Commit**

```bash
git add crates/ironmem/src/collab/state_machine/tests.rs crates/ironmem/tests/mcp_protocol.rs
git commit -m "test(collab): rewire existing v3 tests for Codex-first reorder"
```

---

## Task 5: TDD — write failing test for shortcut ancestry at `review_local`

**Files:**
- Modify: `crates/ironmem/tests/mcp_protocol.rs` — add `test_shortcut_review_local_ancestry_enforced` near the existing shortcut tests.

**Acceptance:**
- Compiles. Fails because the current shortcut-ancestry gate only fires at `CodeReviewFixGlobalPending`, not `CodeReviewLocalPending`. Under new order Claude's `review_local` send with a non-descendant head currently passes when it should fail.

- [ ] **Step 1: Add the test**

```rust
#[tokio::test]
async fn test_shortcut_review_local_ancestry_enforced() {
    let mut h = TestHarness::new_with_git_repo().await;

    // Seed: base -> codex_head -> claude_head are a linear chain.
    let base       = h.git_commit("base").await;
    let codex_head = h.git_commit("codex push").await;
    let claude_head = h.git_commit("claude audit").await;
    // Independent (non-descendant) head:
    h.git_checkout(&base).await;
    let claude_off_branch = h.git_commit("claude unrelated").await;

    let sid = h.collab_start_code_review("/repo", "feat/x", &base, &codex_head, "claude", "shortcut ancestry").await.unwrap();

    // Codex sends a descendant head -> succeeds, advances to CodeReviewLocalPending.
    h.collab_send(&sid, "codex", "review_fix_global",
        &format!(r#"{{"head_sha":"{codex_head}"}}"#)).await.unwrap();
    assert_eq!(h.collab_status(&sid).await.phase, "CodeReviewLocalPending");

    // Claude sends a non-descendant head at review_local -> REJECTED with branch_drift.
    let err = h.collab_send(&sid, "claude", "review_local",
        &format!(r#"{{"head_sha":"{claude_off_branch}"}}"#)).await.unwrap_err();
    assert!(err.to_string().contains("branch_drift"), "expected branch_drift, got: {err}");

    // Claude retries with a descendant head -> succeeds.
    h.collab_send(&sid, "claude", "review_local",
        &format!(r#"{{"head_sha":"{claude_head}"}}"#)).await.unwrap();
    assert_eq!(h.collab_status(&sid).await.phase, "CodeReviewFinalPending");
}
```

Use the actual harness's git helpers; pattern-match the existing shortcut-ancestry test if one exists (gate currently only fires at `CodeReviewFixGlobalPending`).

- [ ] **Step 2: Run — expect failure**

```bash
cargo test --package ironmem --test mcp_protocol test_shortcut_review_local_ancestry_enforced -- --exact
```
Expected: FAIL (the non-descendant `review_local` is currently accepted; assertion `expected branch_drift` fails).

- [ ] **Step 3: Commit**

```bash
git add crates/ironmem/tests/mcp_protocol.rs
git commit -m "test(collab): RED — shortcut ancestry must enforce at review_local under new order"
```

---

## Task 6: Extend shortcut ancestry gate

**Files:**
- Modify: `crates/ironmem/src/mcp/tools/collab_session.rs:303-312`.

**Acceptance:**
- `test_shortcut_review_local_ancestry_enforced` PASSES.
- `cargo test --workspace` green.
- Existing shortcut-ancestry test at `CodeReviewFixGlobalPending` still passes.

- [ ] **Step 1: Read the current gate**

Read `crates/ironmem/src/mcp/tools/collab_session.rs:300-320`.

Confirm shape:
```rust
if matches!(
    (session.phase, &event),
    (crate::collab::Phase::CodeReviewFixGlobalPending, ...CodeReviewFixGlobal ...),
) && session.task_list.is_none()
{
    // ancestry validation using last_head_sha
}
```

- [ ] **Step 2: Extend the match tuple**

Add a second `(Phase, Event)` pair to the `matches!` invocation:

```rust
if matches!(
    (session.phase, &event),
    (crate::collab::Phase::CodeReviewFixGlobalPending, /* CodeReviewFixGlobal pattern */)
        | (crate::collab::Phase::CodeReviewLocalPending, /* ReviewLocal pattern */),
) && session.task_list.is_none()
{
    // existing last_head_sha + git ancestry block stays unchanged
}
```

Read the actual event variant patterns in surrounding code (e.g., `CollabEvent::CodeReviewFixGlobal { .. }`) and mirror the syntax precisely. The error message at line 312 (`"last_head_sha is missing for CodeReviewFixGlobalPending"`) should be parameterized to the actual phase being validated, e.g.:

```rust
return Err(...)::ToolError::Invariant(
    format!("last_head_sha is missing for {phase:?}", phase = session.phase),
);
```

(or whatever format matches surrounding code).

- [ ] **Step 3: Run the new test**

```bash
cargo test --package ironmem --test mcp_protocol test_shortcut_review_local_ancestry_enforced -- --exact
```
Expected: PASS.

- [ ] **Step 4: Run the full workspace**

```bash
cargo test --workspace
```
Expected: all tests pass.

- [ ] **Step 5: Commit**

```bash
git add crates/ironmem/src/mcp/tools/collab_session.rs
git commit -m "feat(collab): extend shortcut ancestry validation to review_local"
```

---

## Task 7: Update Rust source comments

**Files:**
- Modify: `crates/ironmem/src/collab/mod.rs` — module doc-comments describing v3 phase order.
- Modify: `crates/ironmem/src/collab/session.rs` — `new_global_review()` docstring + any phase-order references.
- Modify: `crates/ironmem/src/collab/phase.rs` — variant doc-comments describing v3 ordering.
- Modify: `crates/ironmem/src/collab/state_machine/mod.rs` — module-level + per-arm comments.

**Acceptance:**
- `rg -n 'CodeReviewLocalPending|CodeReviewFixGlobalPending' crates/ironmem/src/collab/` shows no comment that describes Local → Global as the canonical order.
- Each touched file lists the new order in its top doc-comment if it lists the order at all.

- [ ] **Step 1: Audit comments in each target file**

For each of the four files, grep for the old-order tokens within comments:
```bash
for f in crates/ironmem/src/collab/mod.rs \
         crates/ironmem/src/collab/session.rs \
         crates/ironmem/src/collab/phase.rs \
         crates/ironmem/src/collab/state_machine/mod.rs ; do
  echo "== $f =="
  rg -n 'Local.*Global|review_local.*review_fix_global|CodeReviewLocalPending.*CodeReviewFixGlobalPending' "$f"
done
```

- [ ] **Step 2: Rewrite each match to new order**

For each match found, rewrite the comment so it describes the new sequence: `CodeImplementPending → CodeReviewFixGlobalPending → CodeReviewLocalPending → CodeReviewFinalPending → CodingComplete`. If a comment mentions the role of each phase, say:
- `CodeReviewFixGlobalPending` — Codex's first read of the raw post-implementation diff.
- `CodeReviewLocalPending` — Claude's audit of Codex's work via `/ultrareview-local`.

- [ ] **Step 3: Run tests + clippy**

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```
Expected: all pass (comment-only changes).

- [ ] **Step 4: Commit**

```bash
git add crates/ironmem/src/collab/mod.rs \
        crates/ironmem/src/collab/session.rs \
        crates/ironmem/src/collab/phase.rs \
        crates/ironmem/src/collab/state_machine/mod.rs
git commit -m "docs(collab): update Rust source comments for v3 phase reorder"
```

---

## Task 8: Update docs/COLLAB.md

**Files:**
- Modify: `docs/COLLAB.md` — owner table, per-phase descriptions, harness reset rules, `/ultrareview-local` guardrail, deployment subsection.

**Acceptance:**
- Owner table reflects new phase positions.
- Per-phase sections (`CodeReviewFixGlobalPending`, `CodeReviewLocalPending`, `CodeReviewFinalPending`) describe new roles.
- Harness reset rules are scoped by owner (Claude's pre-send vs Codex's receive-side).
- `/ultrareview-local` guardrail names the audit-of-Codex role explicitly.
- New "Deployment" subsection documents drain-or-abort requirement.

- [ ] **Step 1: Update the v3 owner/event table**

Find the table that lists `CodeReviewLocalPending` / `CodeReviewFixGlobalPending` / `CodeReviewFinalPending` rows (under "v3 Coding Phase Model"). Reorder rows so the table lists `CodeReviewFixGlobalPending` before `CodeReviewLocalPending`. Update each row's owner / event / next-phase columns to reflect new transitions.

- [ ] **Step 2: Rewrite per-phase prose**

In the "Phase Model" / "v3 Coding Phase Model" sections, rewrite the descriptions:

- `CodeReviewFixGlobalPending`: "Owner: codex. Codex reads the raw post-implementation diff and applies fixes directly (commit + push). No Claude pre-clean. Send `review_fix_global`."
- `CodeReviewLocalPending`: "Owner: claude. Claude audits Codex's commits via `/ultrareview-local`, fixing code-quality issues found in either Codex's or its own batch impl. Send `review_local`."
- `CodeReviewFinalPending`: (unchanged role).

- [ ] **Step 3: Add owner-scoped pre-send harness reset rules**

In the existing harness-reset section, split into two subsections:

```markdown
### Claude's pre-send harness (Claude-sent v3 topics)

- Skip reset before `task_list` and `implementation_done` (Claude is sole writer).
- Reset to `last_head_sha` before `review_local` (Codex pushed at `review_fix_global` — the only Codex push in v3).
- Skip reset before `final_review` (Claude pushed at `review_local`).

### Codex's pre-send harness (Codex-sent `review_fix_global`)

- Fetch / cat-file / checkout / reset-to-`last_head_sha` before reviewing. This is a receive-side reset — Codex syncs to whatever Claude pushed at `implementation_done` so review uses the canonical post-impl head.
```

- [ ] **Step 4: Update the `/ultrareview-local` anti-removal guardrail**

Replace the existing guardrail paragraph with:

> Under v3 ordering, `/ultrareview-local` audits Codex's `review_fix_global` work plus catches code-quality, maintainability, consistency, and local-read issues that Codex's correctness/security/scope/architecture lens may miss. Its code-quality lens partially overlaps with Codex's lens but does not fully duplicate it. Removing this stage requires a written overlap audit demonstrating that Codex's global reviews catch the code-quality issues `/ultrareview-local` would have flagged AND that the audit-of-Codex role is unnecessary (e.g., Codex's commits never reintroduce issues).

- [ ] **Step 5: Add Deployment subsection**

Under the v3 phase model, add:

```markdown
### Deployment

This change is forward-only; the collab state machine has no protocol-version field. Operational rollout:

1. Pause / avoid starting new coding-phase collab sessions before rollout.
2. Drain existing coding-active sessions to `CodingComplete` / `CodingFailed`, or abort them.
3. Deploy; new sessions start under new ordering.

Sessions stored mid-coding-phase that survive deployment will follow the new transition semantics from their stored phase forward.
```

- [ ] **Step 6: Run acceptance greps**

```bash
rg -n 'CodeReviewLocalPending.{0,80}CodeReviewFixGlobalPending' docs/COLLAB.md
rg -c 'MAX_REVIEW_ROUNDS' docs/COLLAB.md
rg -c 't4_phase_advanced' docs/COLLAB.md
rg -c 'phase=\|round=' docs/COLLAB.md
```
Expected: first command returns 0 matches; PR #55 grep gates 2-4 still pass (1+).

- [ ] **Step 7: Commit**

```bash
git add docs/COLLAB.md
git commit -m "docs(collab): docs/COLLAB.md — v3 reorder (Codex first), owner-scoped reset, audit guardrail, deployment"
```

---

## Task 9: Update .claude-plugin/commands/collab.md

**Files:**
- Modify: `.claude-plugin/commands/collab.md` — v3 dispatch table, pre-send harness reset rules, `/ultrareview-local` row callout.

**Acceptance:**
- Dispatch table rows reflect new phase order.
- Pre-send harness reset rules match docs/COLLAB.md (Claude-side scope).
- Acceptance greps unchanged from PR #55 still pass.

- [ ] **Step 1: Reorder v3 dispatch table**

Find the `Phase → action (v3):` table. Reorder rows so `CodeReviewFixGlobalPending` (Codex's turn) precedes `CodeReviewLocalPending` (Claude's audit turn). Update each row's action text:
- `CodeImplementPending`: Claude action unchanged; expected next phase becomes `CodeReviewFixGlobalPending`.
- `CodeReviewFixGlobalPending`: Codex's turn; dispatcher waits and polls per Codex handoff section.
- `CodeReviewLocalPending`: Claude runs `/ultrareview-local` as audit of Codex's commits; resets to `last_head_sha` per harness rules; sends `review_local`.
- `CodeReviewFinalPending`: Claude (PR creation) — unchanged.

- [ ] **Step 2: Update pre-send harness reset rules**

Find the existing "Reset only when Codex just pushed" passage. Replace with:

```markdown
**Reset to `last_head_sha` only before `review_local`** — Codex pushed at `review_fix_global`, the only Codex push in v3. Skip reset before `task_list`, `implementation_done`, and `final_review` (Claude is the writer in those phases).
```

- [ ] **Step 3: Update `/ultrareview-local` row**

In the `CodeReviewLocalPending` row's note, ensure the anti-removal guardrail wording matches docs/COLLAB.md's updated text.

- [ ] **Step 4: Run acceptance greps**

```bash
rg -n 'CodeReviewLocalPending.{0,80}CodeReviewFixGlobalPending' .claude-plugin/commands/collab.md
rg -c 'MAX_REVIEW_ROUNDS' .claude-plugin/commands/collab.md
rg -c 't4_phase_advanced\b' .claude-plugin/commands/collab.md
rg -c 'backoff\|escalate' .claude-plugin/commands/collab.md
rg -c 'ultrareview-local' .claude-plugin/commands/collab.md
```
Expected: first returns 0; others ≥1 (PR #55 gates still hold).

- [ ] **Step 5: Commit**

```bash
git add .claude-plugin/commands/collab.md
git commit -m "docs(collab): .claude-plugin/commands/collab.md — v3 reorder + reset rules + audit guardrail"
```

---

## Task 10: Update .codex-plugin/prompts/collab.md and collab-batch-impl.md

**Files:**
- Modify: `.codex-plugin/prompts/collab.md` — v3 core rule framing, receive-side reset note, "next receiving-side gate after review_fix_global".
- Modify: `.codex-plugin/prompts/collab-batch-impl.md` — `implementation_done` next-phase reference (was `CodeReviewLocalPending`, must be `CodeReviewFixGlobalPending`).

**Acceptance:**
- Codex sees explicit "no Claude pre-clean" framing.
- Codex's prompt names `CodeReviewLocalPending` as the next receive-side gate (not `CodeReviewFinalPending`).
- `collab-batch-impl.md` lists the correct next phase in both `mechanical_direct` and `subagent-driven` paths.
- Acceptance greps unchanged from PR #55 still pass.

- [ ] **Step 1: Update `.codex-plugin/prompts/collab.md` v3 core rule framing**

Find the "v3 core rule — you write code, not review notes" section. Replace its lead paragraph with:

```markdown
v3 batch mode gives Codex a single coding turn: read the full branch diff and the writing-plans markdown, form your own judgment, apply any fixes directly (commit + push), then send `review_fix_global`. **You see the diff AS-IS — no Claude pre-clean.** `/ultrareview-local` runs *after* you (`CodeReviewLocalPending`), auditing your work; `CodeReviewFinalPending` is Claude's PR turn.

Reaffirm: you do not create PRs. PR creation belongs to Claude at `final_review`.
```

- [ ] **Step 2: Update Codex's pre-send harness receive-side note**

Find the Codex pre-send harness section. Confirm/update wording:

```markdown
Before sending `review_fix_global`, run the receive-side harness: `git fetch` + `git cat-file -e <last_head_sha>` + `git checkout <branch>` + `git reset --hard <last_head_sha>`. This is a receive-side reset that syncs your tree to whatever Claude pushed at `implementation_done`.

After your `review_fix_global` send, the session advances to `CodeReviewLocalPending` (Claude's audit turn), not `CodeReviewFinalPending`.
```

- [ ] **Step 3: Update `.codex-plugin/prompts/collab-batch-impl.md`**

Read the existing file. It documents both `mechanical_direct` and `subagent-driven` paths to `implementation_done`. Find any line that says `implementation_done → CodeReviewLocalPending` and replace with `implementation_done → CodeReviewFixGlobalPending`. Both paths.

- [ ] **Step 4: Run acceptance greps**

```bash
rg -n 'CodeReviewLocalPending.{0,80}CodeReviewFixGlobalPending' .codex-plugin/
rg -c 'review round' .codex-plugin/prompts/collab.md
```
Expected: first returns 0; second ≥1 (PR #55 gate still holds).

- [ ] **Step 5: Commit**

```bash
git add .codex-plugin/prompts/collab.md .codex-plugin/prompts/collab-batch-impl.md
git commit -m "docs(collab): Codex prompts — no-Claude-pre-clean framing + next-phase corrections"
```

---

## Task 11: CHANGELOG + final acceptance

**Files:**
- Modify: `CHANGELOG.md` — `[Unreleased]` `### Changed` entry.

**Acceptance:**
- CHANGELOG entry covers all six bullets from the locked plan.
- All 15 acceptance gates from the canonical plan pass.

- [ ] **Step 1: Add CHANGELOG entry**

Under `[Unreleased]` → `### Changed`, add at the top:

```markdown
- **Collab v3 phase reorder — Codex global review precedes Claude local audit (2026-05-16).**
  Forward-only protocol change. New phase sequence:
  `CodeImplementPending` → `CodeReviewFixGlobalPending` (Codex) → `CodeReviewLocalPending` (Claude audit) → `CodeReviewFinalPending` (Claude PR) → `CodingComplete`.
  Wire-observable through `collab_status.phase` transitions.
  (A) State-machine arms rewired at `crates/ironmem/src/collab/state_machine/mod.rs:172-197`; topic-to-phase event names unchanged.
  (B) Pre-send harness reset rules scoped by harness owner: Claude resets to `last_head_sha` before `review_local` (Codex's only push); Codex keeps its receive-side reset before `review_fix_global`.
  (C) `/ultrareview-local` role shifts to audit-of-Codex; anti-removal guardrail updated.
  (D) Codex prompt framing updated: Codex sees the raw post-implementation diff, no Claude pre-clean.
  (E) Shortcut ancestry validation extended to `(CodeReviewLocalPending, ReviewLocal)` when `task_list.is_none()`; new test `test_shortcut_review_local_ancestry_enforced`.
  (F) **Deployment requirement**: pause / avoid starting new coding-phase collab sessions while existing coding-active sessions are drained or aborted before rollout. No protocol-version migration; sessions surviving deploy follow new semantics from their stored phase forward.
```

- [ ] **Step 2: Run all 15 acceptance gates**

```bash
cd /Users/jeffreycrum/git-repos/ironrace-memory
echo "1+2+3:"; cargo fmt --all -- --check && cargo clippy --workspace --all-targets --all-features -- -D warnings && cargo test --workspace --release
echo "4: v3 phase sequence"; cargo test --package ironmem --lib test_v3_phase_sequence_is_global_then_local -- --exact
echo "5: shortcut audit flow"; cargo test --package ironmem --test mcp_protocol test_shortcut_review_flows_through_audit -- --exact
echo "6: shortcut ancestry"; cargo test --package ironmem --test mcp_protocol test_shortcut_review_local_ancestry_enforced -- --exact
echo "7: v1 force-finalize regression"; cargo test --package ironmem --lib test_v1_force_finalize_still_works_at_max_rounds -- --exact
echo "8: PR #55 grep gates (sample)"; \
  rg -c 'MAX_REVIEW_ROUNDS' docs/COLLAB.md .claude-plugin/commands/collab.md ; \
  rg -c 'review round' .codex-plugin/prompts/collab.md ; \
  rg -c 't4_phase_advanced\b' docs/COLLAB.md
echo "9: docs/COLLAB.md owner table inspection (manual)"
echo "10: ultrareview-local audit-of-Codex wording (manual)"
echo "11: CHANGELOG entry present"; rg -n 'Collab v3 phase reorder' CHANGELOG.md
echo "12: scoped order-drift grep (must be 0)"; rg -c 'CodeReviewLocalPending.{0,80}CodeReviewFixGlobalPending' docs/COLLAB.md .claude-plugin/ .codex-plugin/
echo "13: collab-batch-impl.md uses new next-phase"; rg -n 'implementation_done.*CodeReviewFixGlobalPending' .codex-plugin/prompts/collab-batch-impl.md
echo "14: Rust comment audit (manual)"; rg -n 'CodeReviewLocalPending|CodeReviewFixGlobalPending' crates/ironmem/src/collab/
echo "15: mcp_protocol integration walk (manual)"
```

Address any failures inline before committing.

- [ ] **Step 3: Commit**

```bash
git add CHANGELOG.md
git commit -m "docs(changelog): collab v3 phase reorder — Codex first, deployment drain"
```

- [ ] **Step 4: Push branch**

```bash
git push -u origin feat/collab-v3-reorder-codex-first
```

---

## Verification (end-to-end)

After all tasks complete:

```bash
cd /Users/jeffreycrum/git-repos/ironrace-memory
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --release
rg -n 'CodeReviewLocalPending.{0,80}CodeReviewFixGlobalPending' docs/COLLAB.md .claude-plugin/ .codex-plugin/  # must be empty
rg -n 'CodeReviewLocalPending|CodeReviewFixGlobalPending' crates/ironmem/src/collab/  # spot-check comments are new order
```

End with `implementation_done` send to advance the collab session to `CodeReviewFixGlobalPending` (Codex's audit turn under the new order — note: the session will use whichever order is active when each send fires).
