# Collab `--implementer=codex` Smoke Test Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add one trivial characterization test to `crates/ironrace-core/src/vector.rs` so the IronRace collab v3 batch pipeline (with `--implementer=codex`) has a real, harmless diff to drive end-to-end.

**Architecture:** The diff is intentionally minimal — a single `#[test]` appended to the existing `mod tests` block in `vector.rs`. The test asserts an already-true property of `merge_top_k`, exercising the full Rust gate stack (fmt, clippy `-D warnings`, `cargo test --workspace`) without any production-code change.

**Tech Stack:** Rust 2021, `cargo` (workspace), the existing `merge_top_k` function in `crates/ironrace-core/src/vector.rs`.

**Note on TDD:** Classic TDD writes a failing test first. This plan is a deliberate exception: it's a **characterization test** — the test asserts behavior that is *already* true, so it passes on first run. That's expected and intentional. The smoke test exists to exercise the collab protocol's gates and review pipeline, not to drive new behavior.

---

## File Structure

**Modified:**
- `crates/ironrace-core/src/vector.rs` — append exactly one `#[test]` function inside the existing `#[cfg(test)] mod tests { ... }` block at the bottom of the file. No other lines change.

**Not modified:** Anything else. Especially `crates/ironmem/`, `docs/COLLAB.md`, `.claude-plugin/`, `.codex-plugin/`.

---

## Task 1: Add a characterization test for `merge_top_k`

**Files:**
- Modify: `crates/ironrace-core/src/vector.rs` — inside `mod tests` (the block that starts at line 335 of the current file).

The chosen target is `merge_top_k` (defined at `crates/ironrace-core/src/vector.rs:98`). It is a pure function with no env-var or global-state dependencies, no existing dedicated test of its own, and deterministic behavior. The test asserts: when called with an empty `shard_results` vector, it returns an empty result regardless of `top_k`.

This is true by inspection of the implementation (the outer `for results in shard_results` loop never runs, and `heap` stays empty), so the test is a safe, harmless lock-in.

### Steps

- [ ] **Step 1: Read the current bottom of `vector.rs`**

Confirm the file ends with the existing `#[cfg(test)] mod tests { ... }` block. The new test will be appended *inside* that block, immediately before its closing `}`.

Run:
```bash
sed -n '335,420p' crates/ironrace-core/src/vector.rs
```
Expected: lines 335–419 are the existing test module ending with `}` on line 419, and line 420 is empty / EOF.

- [ ] **Step 2: Append the new test function**

Inside the existing `#[cfg(test)] mod tests { ... }` block (i.e. before its closing `}`), append exactly the following test, with one blank line separating it from the previous `#[test]` function:

```rust
    #[test]
    fn merge_top_k_empty_input_returns_empty() {
        // Characterization: with no shard results to merge, merge_top_k must
        // return an empty Vec regardless of top_k. Asserts existing behavior.
        let result = merge_top_k(Vec::new(), 5);
        assert!(result.is_empty());
    }
```

Indentation is 4 spaces (matches the existing tests in the file). The function body uses `merge_top_k` directly — it's already in scope via the `use super::*;` line at the top of the test module.

- [ ] **Step 3: Run `cargo fmt --all -- --check`**

Run:
```bash
cargo fmt --all -- --check
```
Expected: exits 0, no output. If it fails, run `cargo fmt --all` to fix and re-run the check.

- [ ] **Step 4: Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`**

Run:
```bash
cargo clippy --workspace --all-targets --all-features -- -D warnings
```
Expected: exits 0. Any new warnings are fatal (`-D warnings`); fix or remove them before proceeding.

- [ ] **Step 5: Run the new test in isolation to confirm it passes**

Run:
```bash
cargo test -p ironrace-core --lib vector::tests::merge_top_k_empty_input_returns_empty -- --nocapture
```
Expected: `test result: ok. 1 passed; 0 failed`.

- [ ] **Step 6: Run the full workspace test suite**

Run:
```bash
cargo test --workspace
```
Expected: all crates green. The new test is included in the `ironrace-core` lib test count (one more than before).

- [ ] **Step 7: Stage and commit**

Run:
```bash
git add crates/ironrace-core/src/vector.rs
git status
```
Expected output of `git status`: exactly one modified file, `crates/ironrace-core/src/vector.rs`. **No other files staged or modified.** If anything else appears modified, investigate before committing — it's not part of this task.

Then commit:
```bash
git commit -m "test(ironrace-core): add characterization test for merge_top_k empty input"
```

- [ ] **Step 8: Push to remote**

Run:
```bash
git push origin feat/collab-batch-implementation
```
Expected: push succeeds; remote `feat/collab-batch-implementation` advances by one commit.

### Acceptance criteria (Task 1)

- Exactly one new `#[test]` function added inside the existing `mod tests` in `crates/ironrace-core/src/vector.rs`.
- No production code changed (no edits outside the `mod tests` block).
- `cargo fmt --all -- --check` passes.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` passes.
- `cargo test --workspace` passes (one additional test green in `ironrace-core`).
- Exactly one new commit on `feat/collab-batch-implementation` with the message
  `test(ironrace-core): add characterization test for merge_top_k empty input`.
- Commit pushed to `origin/feat/collab-batch-implementation`.
- No PR opened by this task. Verify before signaling completion:
  ```bash
  gh pr list --head feat/collab-batch-implementation --json number --jq 'length'
  ```
  Expected: `0`.

---

## Verification (end-to-end, post-batch)

The collab protocol drives the rest. After Task 1 completes and Codex sends `implementation_done`:

1. `mcp__ironmem__collab_status` advances through `CodeReviewLocalPending` → `CodeReviewFixGlobalPending` → `CodeReviewFinalPending` → `CodingComplete`.
2. Claude opens a single PR via `gh pr create` during `final_review` (against `main`).
3. The PR diff contains exactly the one-test addition above (plus any incidental fixes from `review_fix_global`, ideally none).
4. No `failure_report` was emitted at any phase.
5. After `final_review`:
   ```bash
   gh pr list --head feat/collab-batch-implementation --json number --jq 'length'
   ```
   Returns `1` — exactly the protocol-opened PR.

If any of the above fails, the run is a *useful* failure: triage from the collab session log and the failing turn's `failure_report` payload.

---

## Out of scope

- Any change to `crates/ironmem/` (collab MCP server).
- Any change to `docs/COLLAB.md`, `.claude-plugin/`, or `.codex-plugin/`.
- New crates, dependencies, or feature flags.
- Behavioral changes to `merge_top_k` or any other production code.
- Additional tests beyond the one specified in Task 1.
