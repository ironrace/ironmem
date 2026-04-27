# Collab `--implementer=codex` Speedup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cut wall time of the `merge_top_k` smoke test (driven through `--implementer=codex`) from 10–15 min to ≤ 5 min by (a) dropping redundant receiver-side pre-work `cargo test` runs and (b) passing `model_reasoning_effort=low` to `mcp__codex__codex` for the batch-impl turn only.

**Architecture:** Pure prompt-file edits — no Rust, no schema, no protocol changes. Two files touched: `.claude-plugin/commands/collab.md` (Claude's dispatch) and `.codex-plugin/prompts/collab.md` (Codex's mirror). Phase 0 removes `cargo test --workspace` from each receiver's pre-send harness, leaving fmt + clippy + branch-drift detection. Phase 1 makes the Codex MCP call phase-aware: low reasoning for `CodeImplementPending` (codex implementer), defaults for review and planning. Verification is the existing `merge_top_k` smoke plan, end-to-end, against a hard 5-min wall-clock cap.

**Tech Stack:** Markdown prompt files. Spec: `docs/superpowers/specs/2026-04-26-collab-codex-implementer-speedup-design.md`. Smoke test: `docs/superpowers/plans/2026-04-25-collab-codex-implementer-smoke-test.md`. Protocol: `docs/COLLAB.md`. Codex MCP tool schema (params accepted): `prompt`, `cwd`, `model`, `config` (overrides `CODEX_HOME/config.toml`), `base-instructions`, `developer-instructions`, `approval-policy`, `sandbox`, `profile`.

---

## File Structure

**Create:**
- None.

**Modify:**
- `.claude-plugin/commands/collab.md` — drop `cargo test --workspace` from the pre-send harness step 4; add a "Codex MCP tuning matrix" subsection; update the Codex-handoff procedure step 2c so the call args branch on `phase` and `implementer`.
- `.codex-plugin/prompts/collab.md` — drop the test command from pre-send harness step 6 (it's a no-op for `CodeImplementPending`/codex-impl, which already skips it; remove for `CodeReviewFixGlobalPending`).

**Test:**
- No unit tests — these are pure prompt-file edits. Verification is the existing `merge_top_k` smoke plan executed end-to-end (Task 4).

---

## Task 1: Drop pre-work `cargo test` from Claude's pre-send harness

**Files:**
- Modify: `.claude-plugin/commands/collab.md` — the "Pre-send Harness Sequence (Claude-owned v3 turns)" subsection (the `1.` … `6.` numbered list immediately after the "v3 Dispatch Loop (Phase → Action Table)" heading; in the current file it starts at line 309).

The current step 4 lists three commands (fmt, clippy, test). We're keeping fmt and clippy (cheap, defend against local-tree drift after reset) and removing the test line. The rationale and protocol invariant that justifies this is documented in the spec.

- [ ] **Step 1: Locate the exact `old_string` for the Edit**

Open `.claude-plugin/commands/collab.md` and confirm the current Pre-send Harness Sequence subsection contains this block verbatim (4-space indented bullets under step 4):

```
4. Run local gates:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test --workspace`
```

If the exact text differs, stop and report — the edit below will fail.

- [ ] **Step 2: Replace step 4 — drop the test line, document why**

Use the Edit tool on `.claude-plugin/commands/collab.md`:

`old_string`:
```
4. Run local gates:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test --workspace`
```

`new_string`:
```
4. Run local gates (pre-work — fmt + clippy only):
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - **No pre-work `cargo test --workspace`.** The receiver just reset to `last_head_sha`, which is the sender-gated commit (every send is post-gated by the sender's harness). Re-running tests on a known-green tree is duplicate work. Branch-drift is already caught at step 2 (`git cat-file -e`). The post-work gate immediately before this turn's `collab_send` runs the full test suite (see step 4 there) — that's where test execution lives.
```

- [ ] **Step 3: Verify the post-work gate description still mentions full tests**

The pre-send harness has TWO test contexts: pre-work (which we just trimmed) and post-work (immediately before each `collab_send`, which is load-bearing). Confirm by grepping:

```bash
grep -n "cargo test --workspace" .claude-plugin/commands/collab.md
```

Expected: **at least one** remaining match — the post-work gate or the table that documents per-phase send actions. If zero matches remain, the edit was over-broad — undo and re-apply more precisely. (The current file structure has `cargo test` referenced in the harness sequence and possibly in the per-phase action table; only the pre-work bullet should be removed.)

- [ ] **Step 4: Confirm Markdown still renders cleanly**

Verify the document still parses as valid Markdown:

```bash
python3 -c "import sys; open('.claude-plugin/commands/collab.md').read(); print('ok')"
```

Expected: `ok`. (This isn't a Markdown lint, just a "file is readable" sanity check. A real lint isn't strictly necessary for this scope, but if you want one, `npx markdownlint-cli2 .claude-plugin/commands/collab.md` works if the project has it; skip otherwise.)

- [ ] **Step 5: Commit**

```bash
git add .claude-plugin/commands/collab.md
git commit -m "perf(collab): drop redundant pre-work cargo test on Claude side

Receiver-side pre-work cargo test is duplicate of the sender's
post-work gate (every send is post-gated by the sender; receiver
resets to last_head_sha = sender-validated commit). Saves one full
cargo test --workspace per Claude-owned receiving turn (review_local,
final_review). fmt + clippy stay (cheap drift defense)."
```

### Acceptance criteria (Task 1)

- The pre-work step 4 in `.claude-plugin/commands/collab.md` lists fmt + clippy only, with a paragraph explaining why pre-work test was removed.
- `grep -n "cargo test --workspace" .claude-plugin/commands/collab.md` still returns at least one match (the post-work / per-phase context).
- One new commit on `feat/collab-batch-implementation` whose only file change is `.claude-plugin/commands/collab.md`.

---

## Task 2: Drop pre-work `cargo test` from Codex's pre-send harness

**Files:**
- Modify: `.codex-plugin/prompts/collab.md` — the "Pre-send Harness Sequence (v3 turns only)" subsection (numbered list `1.` … `7.` immediately after the heading "## v3 Dispatch Loop (Phase → Action Table)"; currently starts around line 161).

Codex's pre-send harness has step 6: *"Run the project's test command (language-appropriate: cargo test, pytest, npm test, go test ./..., etc). Record failures."* For `CodeReviewFixGlobalPending`, this duplicates the test run Claude just did before sending `implementation_done` or `review_local`. (For `CodeImplementPending` the existing batch-implementation block already says to skip step 6, so that's already handled; we're not changing that.)

- [ ] **Step 1: Locate the exact `old_string` for the Edit**

Open `.codex-plugin/prompts/collab.md` and confirm step 6 reads verbatim:

```
6. Run the project's test command (language-appropriate: `cargo test`,
   `pytest`, `npm test`, `go test ./...`, etc). Record failures.
```

If the wording differs, stop and report.

- [ ] **Step 2: Replace step 6 — remove the test command, keep the slot**

Use the Edit tool on `.codex-plugin/prompts/collab.md`:

`old_string`:
```
6. Run the project's test command (language-appropriate: `cargo test`,
   `pytest`, `npm test`, `go test ./...`, etc). Record failures.
```

`new_string`:
```
6. **No pre-work test command.** The receiver just reset to `last_head_sha`,
   which is the sender's post-work-gated commit (every send is post-gated
   by the sender's harness). Re-running tests on a known-green tree is
   duplicate work. Branch-drift was already caught in step 4
   (`git cat-file -e`). The phase-specific action below is responsible
   for running the project's test command **after** any fixes are
   applied, immediately before the outgoing `collab_send`.
```

- [ ] **Step 3: Verify the existing `CodeImplementPending` skip note still references step 6 correctly**

The batch-implementation subsection already says "skip the test command in step 6 — there's no prior commit to validate yet beyond what Claude pushed at last_head_sha." Confirm that text still makes sense alongside the new step 6 (it should — the new step 6 is a stronger version of the same skip, but the batch-impl reference is still valid because step 6 still exists, just with different content). Grep:

```bash
grep -n "skip the test command in step 6" .codex-plugin/prompts/collab.md
```

Expected: one match. The reference is intentionally left in place — it's still accurate (step 6 is now a no-op anyway, so "skip" is a no-op). If the line is gone, the file structure changed and you'll need to re-orient.

- [ ] **Step 4: Verify the post-work test instruction in the batch-impl block is still present**

Grep for the post-work final-gates instruction:

```bash
grep -n "Run final gates" .codex-plugin/prompts/collab.md
```

Expected: at least one match — the line in the "Batch implementation (codex-implementer)" section ("Run final gates (project-appropriate: `cargo test`, `pytest`, etc)"). If this is missing, do not commit — that's the load-bearing post-work gate and it must stay.

- [ ] **Step 5: Confirm Markdown is still readable**

```bash
python3 -c "open('.codex-plugin/prompts/collab.md').read(); print('ok')"
```

Expected: `ok`.

- [ ] **Step 6: Commit**

```bash
git add .codex-plugin/prompts/collab.md
git commit -m "perf(collab): drop redundant pre-work cargo test on Codex side

Same rationale as the Claude-side change: receiver-side pre-work test
duplicates sender's post-work gate. Affects review_fix_global
(batch-impl already skipped pre-work test). Post-work final gates
stay — those are the actual safety belt."
```

### Acceptance criteria (Task 2)

- Step 6 of the pre-send harness in `.codex-plugin/prompts/collab.md` no longer instructs Codex to run a test command pre-work.
- The "skip the test command in step 6" reference in the batch-implementation block is still present.
- The "Run final gates" instruction in the batch-implementation block is still present (post-work gate intact).
- One new commit whose only file change is `.codex-plugin/prompts/collab.md`.

---

## Task 3: Make the Codex MCP call phase-aware (low reasoning for batch impl)

**Files:**
- Modify: `.claude-plugin/commands/collab.md` — the "Codex handoff — synchronous MCP invocation" subsection (currently around lines 364–422). Specifically the procedure block describing the JSON args passed to `mcp__codex__codex` (currently around lines 380–422; the literal JSON example is around lines 395–403).

Today the dispatch loop calls `mcp__codex__codex` with only `prompt` and `cwd`. The MCP tool also accepts `config` (overrides `CODEX_HOME/config.toml`). We add a phase-aware tuning matrix: pass `config: {"model_reasoning_effort": "low"}` only when the next Codex turn is the batch-implementation turn (`CodeImplementPending` with `implementer == "codex"`); leave defaults for review and planning turns.

- [ ] **Step 1: Add the "Codex MCP tuning matrix" subsection**

Open `.claude-plugin/commands/collab.md`. Find the heading line:

```
### Codex handoff — synchronous MCP invocation
```

Use the Edit tool to insert a new subsection **immediately before** that heading. The new content:

`old_string`:
```
### Codex handoff — synchronous MCP invocation
```

`new_string`:
```
### Codex MCP tuning matrix

Codex's default reasoning effort is the dominant latency cost on long
silent grinds. The MCP tool accepts a `config` argument that overrides
`CODEX_HOME/config.toml`. Use this matrix to pick the per-phase config
for every Codex handoff. Don't blanket-apply low reasoning — review
and planning turns are where the second-opinion value lives, and a
shallow reviewer defeats the protocol's design.

| Phase from `collab_status` | `implementer` | Config override | Rationale |
|---|---|---|---|
| `CodeImplementPending` | `"codex"` | `{ "model_reasoning_effort": "low" }` | Batch impl follows an approved plan — mechanical |
| `CodeReviewFixGlobalPending` | (any) | **none** (defaults preserved) | Reviewer judgment must not be shallow |
| `PlanParallelDrafts` | (any) | **none** | Planning needs reasoning |
| `PlanCodexReviewPending` | (any) | **none** | Plan review needs reasoning |
| `CodeImplementPending` | `"claude"` | n/a — Codex isn't owner | Claude runs subagents on its side; no Codex MCP call |

Read `phase` and `implementer` from the `collab_status` you fetched at
the top of the dispatch step (you already do this); branch on them
when constructing the MCP call below.

### Codex handoff — synchronous MCP invocation
```

- [ ] **Step 2: Update the MCP-call JSON example to show the conditional `config`**

Find the existing JSON example block. The current text is:

```
   c. Call:
      ```json
      {
        "name": "mcp__codex__codex",
        "arguments": {
          "prompt": "<resolved prompt text>",
          "cwd": "<repo_path from collab_status>"
        }
      }
      ```
```

Use the Edit tool to replace it with a phase-aware version:

`old_string`:
```
   c. Call:
      ```json
      {
        "name": "mcp__codex__codex",
        "arguments": {
          "prompt": "<resolved prompt text>",
          "cwd": "<repo_path from collab_status>"
        }
      }
      ```
```

`new_string`:
```
   c. Build the `arguments` object:
      - Always include `prompt` (the resolved Codex slash-command text)
        and `cwd` (the session's `repo_path`).
      - Look up the row in the "Codex MCP tuning matrix" above using
        the `phase` and `implementer` you already have from
        `collab_status`. If that row specifies a config override, add
        a `config` field with that exact value. If the row says
        "none", omit `config` entirely.

      Example for the batch-impl row (`CodeImplementPending` +
      `implementer == "codex"`):
      ```json
      {
        "name": "mcp__codex__codex",
        "arguments": {
          "prompt": "<resolved prompt text>",
          "cwd": "<repo_path from collab_status>",
          "config": { "model_reasoning_effort": "low" }
        }
      }
      ```

      Example for any other Codex-owned phase (review, planning):
      ```json
      {
        "name": "mcp__codex__codex",
        "arguments": {
          "prompt": "<resolved prompt text>",
          "cwd": "<repo_path from collab_status>"
        }
      }
      ```

      Do not pass `model` or any other override — only `config` per the
      matrix. Model swap is intentionally out of scope.
```

- [ ] **Step 3: Verify both JSON examples are syntactically valid**

```bash
python3 -c '
import json, re, pathlib
text = pathlib.Path(".claude-plugin/commands/collab.md").read_text()
blocks = re.findall(r"```json\s*(\{.*?\})\s*```", text, re.DOTALL)
for i, b in enumerate(blocks):
    try:
        json.loads(b)
        print(f"block {i}: OK")
    except Exception as e:
        print(f"block {i}: BAD — {e}\n{b!r}")
'
```

Expected: every JSON block prints `OK`. If any block fails to parse, fix the indentation/quoting in the Markdown and re-run.

- [ ] **Step 4: Confirm the matrix is reachable from the dispatch loop**

Grep for the references:

```bash
grep -nE "tuning matrix|model_reasoning_effort" .claude-plugin/commands/collab.md
```

Expected: at least three matches — the "tuning matrix" heading, the matrix table, and the JSON example with `model_reasoning_effort`.

- [ ] **Step 5: Commit**

```bash
git add .claude-plugin/commands/collab.md
git commit -m "perf(collab): pass model_reasoning_effort=low for codex batch impl

Add phase-aware Codex MCP tuning matrix. Override
model_reasoning_effort to 'low' only on CodeImplementPending +
implementer=codex (mechanical plan execution). Preserve defaults
on review (CodeReviewFixGlobalPending) and planning (v1) turns —
reviewer judgment is the design's whole second-opinion value.

Spec: docs/superpowers/specs/2026-04-26-collab-codex-implementer-speedup-design.md"
```

### Acceptance criteria (Task 3)

- New "Codex MCP tuning matrix" subsection is present in `.claude-plugin/commands/collab.md` immediately before "Codex handoff — synchronous MCP invocation".
- The matrix lists at least the four rows above (batch-impl, review, two planning phases).
- The MCP-call JSON examples in the handoff procedure include both a "with `config`" example (batch impl) and a "without `config`" example (other phases).
- All ` ```json ` blocks in the file parse as valid JSON via the script in Step 3.
- One new commit whose only file change is `.claude-plugin/commands/collab.md`.

---

## Task 4: Run the smoke test, measure, decide

**Files:**
- Read: `docs/superpowers/plans/2026-04-25-collab-codex-implementer-smoke-test.md` (the existing `merge_top_k` plan).
- No file modifications in this task. This task is the verification gate that decides whether Phases 0+1 are sufficient or we need to escalate.

This task IS the test. The smoke plan exercises the full collab pipeline end-to-end. Measure wall time. Apply the decision rules from the spec.

- [ ] **Step 1: Confirm working tree is clean and on the right branch**

```bash
git status
git branch --show-current
```

Expected: clean working tree, branch `feat/collab-batch-implementation`. If dirty, stash or commit before starting (a fresh smoke session expects a known starting state).

- [ ] **Step 2: Confirm the prior smoke test's diff is not already applied**

The smoke plan adds `merge_top_k_empty_input_returns_empty` to `crates/ironrace-core/src/vector.rs`. If a previous run already landed it, the smoke test won't be a clean trial.

```bash
grep -n "merge_top_k_empty_input_returns_empty" crates/ironrace-core/src/vector.rs && echo "ALREADY APPLIED" || echo "NOT YET APPLIED"
```

Expected: `NOT YET APPLIED`. If `ALREADY APPLIED`, either revert the prior commit or pick a different trivial test in the smoke plan before proceeding (don't proceed with a no-op smoke).

- [ ] **Step 3: Note start time and kick off the smoke session**

Record `T_start` (wall clock, in seconds since epoch is fine):

```bash
date +%s > /tmp/codex-smoke-start.txt
cat /tmp/codex-smoke-start.txt
```

Then in the Claude session, invoke the collab start with the merge_top_k task. Use the description from the smoke plan:

```
/collab start --implementer=codex Add a characterization test for merge_top_k empty input in crates/ironrace-core/src/vector.rs per docs/superpowers/plans/2026-04-25-collab-codex-implementer-smoke-test.md
```

The session will run autonomously (drafts, synthesis, plan-finalize, batch impl, local review, global review, PR open).

- [ ] **Step 4: Watch the clock — enforce the 5-minute hard cap**

Set a timer for 5 minutes from `T_start`. If at any point during the run wall time exceeds 5 minutes, abort:

1. Interrupt the current `mcp__codex__codex` call (Ctrl+C in the Claude terminal).
2. From Claude's session, send a failure_report:
   ```
   mcp__ironmem__collab_send with
     sender="claude",
     topic="failure_report",
     content='{"coding_failure":"manual_kill: latency_budget_exceeded"}'
   ```
   The branch-drift carve-out admits this from a non-owner. Session moves to `CodingFailed`.
3. Skip to Step 7 (escalate).

- [ ] **Step 5: On `CodingComplete`, capture stop time and validate the artifact**

When `mcp__ironmem__collab_status` returns `phase: "CodingComplete"`:

```bash
echo $(($(date +%s) - $(cat /tmp/codex-smoke-start.txt))) > /tmp/codex-smoke-elapsed.txt
echo "Elapsed: $(cat /tmp/codex-smoke-elapsed.txt) seconds"
```

Validate the artifact:

```bash
gh pr list --head feat/collab-batch-implementation --json number,title --jq '.'
```

Expected: exactly one PR, title roughly matching "test(ironrace-core): add characterization test for merge_top_k empty input" (or whatever the smoke plan's commit message produced).

```bash
gh pr diff $(gh pr list --head feat/collab-batch-implementation --json number --jq '.[0].number') | head -40
```

Expected: the diff is exactly the one `#[test] fn merge_top_k_empty_input_returns_empty` insertion described in the smoke plan, plus any incidental review_fix_global no-op (ideally none).

- [ ] **Step 6: Apply decision rule**

Read the elapsed seconds:

```bash
ELAPSED=$(cat /tmp/codex-smoke-elapsed.txt)
echo "T = $ELAPSED seconds (cap = 300)"
```

**If `ELAPSED <= 300` AND the PR diff is clean AND no `failure_report` was emitted:** Phases 0+1 succeeded. Stop here. Report results to the user (elapsed time, PR URL, list of unexpected diffs if any). Do not implement Phases 2/3.

**Otherwise (`ELAPSED > 300`, or `failure_report` emitted, or diff contains unexpected changes):** Phases 0+1 are insufficient. Skip to Step 7 (escalate).

- [ ] **Step 7: On failure — surface the data, stop, do not auto-escalate**

Do **not** auto-implement Phase 2/3 from this plan. Instead, gather diagnostics:

```bash
echo "Elapsed seconds: $(cat /tmp/codex-smoke-elapsed.txt 2>/dev/null || echo 'aborted before completion')"
git log --oneline -10
mcp__ironmem__collab_status  # if session still queryable
```

Report to the user with:
1. Wall-clock elapsed (or "aborted at N seconds").
2. Phase the session was in when killed (or "CodingComplete but over budget").
3. Whether the PR was opened (and if so, its diff).
4. Recommendation: re-open the spec for Phase 2 design (prompt trimming, redundant `git fetch`, etc.) or jump to Phase 3 (background `codex exec`) based on the failure mode.

The user explicitly approved the layered approach with stops between phases. Phase 2/3 require a fresh design pass — not a continuation of this plan.

### Acceptance criteria (Task 4)

- A measured wall-clock time (`/tmp/codex-smoke-elapsed.txt` or equivalent) recorded for the full smoke run.
- A clear decision: success (≤ 5 min, clean diff) or failure (with concrete reason — over budget, failure_report, or bad diff).
- On success: PR URL captured, no further changes made, plan complete.
- On failure: diagnostics gathered, user informed, no auto-escalation. Plan execution ends; next steps are a separate design conversation.

---

## Out of scope

- Adding `model` overrides or swapping Codex's model. Reasoning effort alone is the cheaper, safer first lever.
- Adding `created_at` / `completed_at` columns to the collab DB. Useful for future tuning but separate concern.
- Designing Phase 2 (prompt trimming, sender-side `git fetch` removal, etc.) or Phase 3 (background `codex exec` for live progress). Both are gated by Task 4's outcome and require a fresh spec pass if we get there.
- Any change to the Claude-implementer path (`/collab start <task>` without `--implementer=codex`). Different bottlenecks.
- Any change to the protocol, state machine, or the `mcp__codex__codex` MCP server itself.
