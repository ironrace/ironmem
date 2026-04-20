---
description: Start or join an IronRace bounded planning session with Codex, auto-flowing into v3 coding if enabled. Covers v1 planning and v3 per-task linear → global review → PR handoff. Usage — /collab start <task>  |  /collab join <session_id>
argument-hint: start <task> | join <session_id>
---

<!-- DERIVED FROM docs/COLLAB.md — protocol changes must update:
     - docs/COLLAB.md (spec)
     - .claude-plugin/commands/collab.md (this file)
     - .codex-plugin/prompts/collab.md (Codex mirror) -->


You are participating in the IronRace bounded collaboration protocol (v1 planning
+ v3 coding). Full spec: `docs/COLLAB.md`. The user has invoked `/collab` with
arguments:

$ARGUMENTS

Parse the first word of `$ARGUMENTS` as the subcommand and behave as follows.

## `start <task>`

Everything except the task is inferred — never ask the user for paths or
branch names.

1. Resolve defaults:
   - `repo_path` ← output of `git rev-parse --show-toplevel` (run via Bash).
   - `branch` ← output of `git branch --show-current`.
   - `initiator` ← `"claude"` (this is Claude's terminal).
   - `task` ← the remainder of `$ARGUMENTS` after the word `start`.
2. Call `mcp__ironmem__collab_start` with those four fields.
3. Tell the user, in a single line they can copy-paste into Codex's terminal:

   ```
   Run in Codex: /collab join <session_id>
   ```

4. Enter Plan Mode and draft your first plan for `<task>` — the draft is
   yours alone, Codex cannot see it. When you have the user's approval in
   Plan Mode, call `mcp__ironmem__collab_send` with
   `sender="claude"`, `topic="draft"`, `content=<the plan text>`.
5. After the draft is sent, begin the v1 planning loop (below). After the
   plan locks (`PlanLocked`), the session automatically flows into the v3
   coding bridge (no separate invocation needed).

## `join <session_id>`

1. Store `<session_id>` as the current collab session — reuse it on every
   subsequent `collab_*` call without re-prompting the user.
2. `agent` / `sender` / `receiver` ← `"claude"` (still Claude's terminal;
   in Codex's terminal this would be `"codex"`, handled by the Codex side).
3. Call `mcp__ironmem__collab_status` to read `task`,
   `phase`, and `current_owner`. Report the task and phase to the user.
4. Branch on the returned `phase`:
   - **v1 active** (`PlanParallelDrafts` .. `PlanClaudeFinalizePending`) →
     enter the v1 planning loop (see below).
   - **`PlanLocked` pre-task_list** (final_plan_hash set, no task_list yet) →
     enter the v3 bridge (see "v3 bridge" section).
   - **v3 active** (`CodeImplementPending` .. `CodeReviewFinalPending`) →
     enter the v3 dispatch loop at the current phase (see "v3 dispatch loop").
   - **v3 terminal** (`CodingComplete` / `CodingFailed`) →
     report the status and exit.

## Dispatch Loop Structure

Both v1 and v3 share a common dispatch loop:

```text
loop:
  status = collab_status(session_id)

  if session_ended or phase in terminal_set:
    exit and report to user

  if current_owner == "codex":
    invoke mcp__codex__codex with "/collab join <session_id>"
      (see "Codex handoff — synchronous MCP invocation" below)
    loop  # re-read status when Codex returns

  # current_owner == "claude"
  recv(session_id, "claude") → ack each message
  act on phase (send exactly one message per iteration)
  loop
```

`wait_my_turn` is only needed to bridge brief race windows where the
server is still writing state after a send. Do NOT use it as a
wait-for-Codex mechanism — Codex isn't polling, Claude drives it.

Terminal sets:
- **v1**: `{PlanLocked}` (until `task_list` is sent)
- **v3**: `{CodingComplete, CodingFailed}`

Once `task_list` has been sent, `PlanLocked` is no longer terminal: the session
stays active and the terminal set flips to the v3 set above. The v3 bridge
(below) sends `task_list` and falls directly into the v3 dispatch loop, so the
planning loop never re-polls at `PlanLocked` post-`task_list`.

## v1 Planning Loop (Phase → Action Table)

Repeat the dispatch loop with these actions:

| Phase | What to do (is_my_turn == true) |
|---|---|
| `PlanParallelDrafts` | Your draft was already sent from the `start` branch. is_my_turn should be false here — if true, verify with `collab_status`. If `collab_status` confirms Claude is the owner in a Codex-owned phase, this is a protocol-level anomaly — exit the loop and report to the user; do not attempt a send. |
| `PlanSynthesisPending` | **Do not ask the user.** Merge both drafts (or revise prior canonical on revision rounds) into a canonical plan. Call `collab_send` with `sender="claude"`, `topic="canonical"`, `content=<plan text>` (plain text — `draft` and `canonical` are the only v1 topics that are NOT JSON-wrapped). |
| `PlanCodexReviewPending` | Codex's turn. is_my_turn should be false — if true, verify with `collab_status`. If the inconsistency persists, exit the loop and report to the user. |
| `PlanClaudeFinalizePending` | **Enter Plan Mode.** Produce the final plan, incorporating Codex's review notes unless they conflict with user intent. Get user approval. Call `collab_send` with `sender="claude"`, `topic="final"`, `content=<JSON string of {"plan":"<full text>"}>` (v1 `final` is the only v1 topic wrapped in JSON). After send, `PlanLocked` is reached. |

Rationale: the user approves only at finalization—the commit point. Everything
before (drafts, synthesis, revisions) runs autonomously.

## v3 Bridge: PlanLocked → CodeImplementPending

Once `PlanLocked` is reached with `final_plan_hash` set and no `task_list` yet:

1. Read `final_plan_hash` and `final_plan` from `collab_status(session_id)`.
   `final_plan` is the JSON string `{"plan":"<full text>"}` Claude previously
   sent; parse it to recover the approved plan body. Read the current `HEAD`
   SHA via `git rev-parse HEAD` (the session record does not carry a HEAD
   field — that's the harness's responsibility).
2. **Enter Plan Mode.** Build a task list from the locked plan:
   - Each task has `id` (strictly ordered starting at 1), `title`, and
     `acceptance` (list of acceptance criteria, ≥1 per task).
   - Document what success looks like for each task.
3. Get user approval in Plan Mode.
4. Build `task_list` JSON:
   ```json
   {
     "plan_hash": "<final_plan_hash>",
     "base_sha": "<current HEAD>",
     "head_sha": "<current HEAD>",
     "tasks": [
       {
         "id": 1,
         "title": "...",
         "acceptance": ["criterion 1", "criterion 2"]
       },
       ...
     ]
   }
   ```
5. Call `collab_send(sender="claude", topic="task_list",
   content=<JSON string>)`. Session advances to `CodeImplementPending`.
6. Fall through into the v3 dispatch loop.

## v3 Dispatch Loop (Phase → Action Table)

v3 collapses the coding loop to three linear turns per task and three linear
turns at the global stage. There are no verdict/debate turns: Codex reviews
and fixes in a single step, Claude does a final pass, and the next task
starts. This structurally prevents Claude from steering Codex's conclusions.

For every coding-active phase, execute this pre-send harness sequence before
building the payload:

**Pre-send Harness Sequence (v3 turns only):**
1. `collab_status(session_id)` → read `last_head_sha`.
2. `git fetch` + `git cat-file -e <last_head_sha>^{commit}` — if the commit
   is missing locally after fetch, send `failure_report` with
   `coding_failure` field containing
   `"branch_drift: last_head_sha=<sha> not found in local repo"` and exit
   the loop (do not retry silently).
3. `git reset --hard <last_head_sha>` (or equivalent) so Claude starts from
   the exact commit Codex/Claude last pushed. Never build on top of
   unsynced local state.
4. Run local gates **for every Claude-owned coding turn** —
   `CodeImplementPending`, `CodeFinalPending`, `CodeReviewLocalPending`,
   `CodeReviewFinalPending`:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test --workspace`
5. On any gate failure, send `failure_report` with concrete error message
   (no silent retry). Include the exact error output.
6. Otherwise, proceed to the phase-specific action below.

| Phase | What to do (is_my_turn == true) |
|---|---|
| `CodeImplementPending` | **Run pre-send harness.** Write code for the current task. Commit and push. Call `collab_send` with `sender="claude"`, `topic="implement"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. Payload carries ONLY `head_sha` — no review notes, no self-critique, no guidance for Codex. Codex reads the diff and forms its own judgment. |
| `CodeReviewFixPending` | Codex's turn. is_my_turn should be false. If `collab_status` confirms Claude is the owner, exit the loop and report the anomaly; do not attempt a send. |
| `CodeFinalPending` | **Run pre-send harness.** Codex has pushed fixes (or a no-op commit if clean); your local working copy was reset to `last_head_sha` by the harness. Optionally make final adjustments — Claude always gets the last word on each task. Commit + push if you change anything. Re-run gates. Call `collab_send` with `sender="claude"`, `topic="final"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. Advances to the next task's `CodeImplementPending`, or to `CodeReviewLocalPending` after the last task. |
| `CodeReviewLocalPending` | **Run pre-send harness, then `/ultrareview-local` on the full task stack.** Fix any CRITICAL/HIGH inline (commit + push). Call `collab_send` with `sender="claude"`, `topic="review_local"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. |
| `CodeReviewFixGlobalPending` | Codex's turn. is_my_turn should be false. If `collab_status` confirms Claude is the owner, exit the loop and report the anomaly. |
| `CodeReviewFinalPending` | **Run pre-send harness (gates before final).** Codex has pushed any global fixes (or a no-op commit); your local working copy was reset to `last_head_sha` by the harness. Optionally tweak. Re-run gates. Then **enter Plan Mode**: draft PR title (under 70 chars) and body (summary + test plan derived from task list + gate results). Get user approval. Then `gh pr create --base <base_branch> --head <current branch> --title <approved title> --body <approved body>`. If `gh pr create` fails, send `failure_report` with `coding_failure: "pr_create_failed: <error>"` — no silent retry. On success, capture `pr_url` and call `collab_send` with `sender="claude"`, `topic="final_review"`, `content=<JSON {"head_sha":"<current HEAD>","pr_url":"<https url>"}>`. Session advances directly to `CodingComplete`. Exit loop. |

After each send in v3, loop back to polling. The loop continues until
`phase in {CodingComplete, CodingFailed}` or `session_ended`.

### Anti-puppeteering rules (v3)

v3 structurally removes the `verdict` and `comment` turns that v2 used,
but a few behavioral rules remain:

- The `implement` and `final` payloads carry ONLY `head_sha`. Do not
  embed review notes, self-critique, or instructions for Codex in any
  other field — there are no other fields.
- When driving Codex via `mcp__codex__codex`, the `prompt` argument is
  the verbatim expanded `.codex-plugin/prompts/collab.md` with
  `$ARGUMENTS` substituted. Nothing more. Do not append session
  context, state summary, or recommendations about what Codex should
  conclude. See the handoff section below.
- Codex's `review_fix` commit stands as its own judgment. If Claude
  disagrees with a fix during `CodeFinalPending`, the right response is
  to amend the code and commit — not to re-litigate in prose.

### Codex handoff — synchronous MCP invocation

**Whenever `current_owner == "codex"` in a coding-active or planning-active
phase, drive Codex's turn inline via the Codex MCP tool.** Codex CLI
sessions are one-shot and do not sustain `wait_my_turn` loops across
handoffs, so Claude is the single control loop: polling when it's Claude's
turn, spawning Codex via MCP when it's Codex's turn. This applies in three
contexts:

- **After a Claude `collab_send`** that flips the owner to Codex.
- **On `/collab join`** when status returns `current_owner == "codex"`.
- **Inside the dispatch loop** whenever a `collab_status` check shows
  `current_owner == "codex"` — do not fall back to a bare `wait_my_turn`
  poll, because no Codex session is running to change the state.

Procedure:

1. Read a fresh `collab_status`. If `current_owner == "claude"` or
   `phase` is terminal, skip this step and resume polling / exit.
2. If `current_owner == "codex"` and phase is non-terminal, expand the
   Codex slash command locally and pass the resolved prompt to the MCP
   tool. **`codex mcp-server` does not read `.codex-plugin/prompts/` —
   the literal `/collab join <sid>` string would be treated as ordinary
   user text.** So:

   a. Read `.codex-plugin/prompts/collab.md` from the collab repo
      (`/Users/jeffreycrum/git-repos/ironrace-memory/.codex-plugin/prompts/collab.md`
      — this repo holds the canonical prompt regardless of the target
      `repo_path`).
   b. Substitute `$ARGUMENTS` in that file with `join <session_id>`.
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

   **The `prompt` argument is the verbatim expanded
   `.codex-plugin/prompts/collab.md` with `$ARGUMENTS` substituted —
   nothing more.** Do not append, prepend, or inline any session
   context, state summary, recap of Claude's last message, or
   instructions about what Codex should conclude. Codex reads state
   via its own `collab_status` and `recv` calls and must form its
   own judgment. Hand-crafted prompts that steer Codex toward a
   conclusion ("withdraw objections", "this is pro-forma", "Claude
   intends to fix everything") collapse the review into a
   rubber-stamp and defeat the point of an independent second pass.

   Block on the result. Codex executes its phase-specific action
   (review+fix, global review+fix, etc.) and returns when the session
   flips back to a Claude-owned phase or terminates.
3. When `mcp__codex__codex` returns, resume the dispatch loop at
   `collab_status`. The next iteration will see either a Claude-owned
   phase or a terminal condition.

Applies to every Codex-owned phase: v1 `PlanParallelDrafts` (Codex
drafting), v1 `PlanCodexReviewPending`, v3 `CodeReviewFixPending`,
v3 `CodeReviewFixGlobalPending`.

If `mcp__codex__codex` is not registered, fall back to the legacy flow:
tell the user to run `/collab join <session_id>` in a Codex terminal,
then `ScheduleWakeup` and resume polling.

## Invariants — do not violate

- **Never** call `mcp__ironmem__collab_end` during any active phase. Rejected in:
  - v1 active: `PlanParallelDrafts`, `PlanSynthesisPending`,
    `PlanCodexReviewPending`, `PlanClaudeFinalizePending`.
  - v3 active: `CodeImplementPending`, `CodeReviewFixPending`,
    `CodeFinalPending`, `CodeReviewLocalPending`,
    `CodeReviewFixGlobalPending`, `CodeReviewFinalPending`.

  Only valid from `PlanLocked` pre-`task_list` (abandon plan), `CodingComplete`,
  or `CodingFailed`.
- **Never** peek at Codex's draft before sending your own during
  `PlanParallelDrafts`. The server enforces blind-draft in `recv`.
- **Plan Mode gates only at: v1 initial `draft` (from `/collab start`),
  v1 `final` (`PlanClaudeFinalizePending`), v3 `task_list` (bridge), and
  v3 `final_review` (`CodeReviewFinalPending`, PR creation).** Every other
  turn runs autonomously.
- **Every v3 `collab_send` payload is JSON** per the matrix in `docs/COLLAB.md`.
  Never send prose payloads for v3 topics.
- **`head_sha` in every v3 payload must be the current `HEAD` AFTER any
  commit/push that preceded this turn.** The server records branch progress
  via `head_sha`.
- **Branch-drift carve-out:** `failure_report` may be sent by either agent at
  any time during a coding-active phase, independent of `current_owner`. It is
  the only topic that bypasses the owner check. A `coding_failure` prefixed
  `"branch_drift:"` is the canonical drift signal; do not suppress it.
- If the user interrupts with a question or correction during v1, answer it
  inside Plan Mode and incorporate it into the next send. During v3, gate at
  Plan Mode points only (`task_list`, `final_review`); all other turns are
  autonomous.

## Unknown subcommand

If `$ARGUMENTS` does not start with `start` or `join`, tell the user:

```
Usage: /collab start <task>  |  /collab join <session_id>
```
