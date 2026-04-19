---
description: Start or join an IronRace bounded planning session with Codex. Usage — /collab start <task>  |  /collab join <session_id>
argument-hint: start <task> | join <session_id>
---

You are participating in the IronRace bounded planning protocol (v1). Full
spec: `docs/COLLAB.md`. The user has invoked `/collab` with arguments:

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
2. Call `mcp__ironrace-memory__ironmem_collab_start` with those four fields.
3. Tell the user, in a single line they can copy-paste into Codex's terminal:

   ```
   Run in Codex: /collab join <session_id>
   ```

4. Enter Plan Mode and draft your first plan for `<task>` — the draft is
   yours alone, Codex cannot see it. When you have the user's approval in
   Plan Mode, call `mcp__ironrace-memory__ironmem_collab_send` with
   `sender="claude"`, `topic="draft"`, `content=<the plan text>`.
5. After the draft is sent, begin the autonomous planning loop (see below).

## `join <session_id>`

1. Store `<session_id>` as the current collab session — reuse it on every
   subsequent `ironmem_collab_*` call without re-prompting the user.
2. `agent` / `sender` / `receiver` ← `"claude"` (still Claude's terminal;
   in Codex's terminal this would be `"codex"`, handled by the Codex side).
3. Call `mcp__ironrace-memory__ironmem_collab_status` to read `task`,
   `phase`, and `current_owner`. Report the task to the user.
4. Enter the autonomous planning loop.

## Autonomous planning loop (both start and join)

Repeat until `phase == "PlanLocked"` or `session_ended == true`:

1. `mcp__ironrace-memory__ironmem_collab_wait_my_turn` with
   `agent="claude"`, `timeout_secs=30`. This long-polls server-side.
2. If `session_ended` or `phase == "PlanLocked"`, exit the loop.
3. If `is_my_turn == false`, loop again.
4. `mcp__ironrace-memory__ironmem_collab_status` → read `phase`,
   `current_owner`, `review_round`.
5. `mcp__ironrace-memory__ironmem_collab_recv` with `receiver="claude"`.
   Ack each message via `mcp__ironrace-memory__ironmem_collab_ack`.
6. Act based on `phase`:
   - `PlanParallelDrafts` — you already sent your draft in step 4 above;
     just loop.
   - `PlanSynthesisPending` — enter Plan Mode, synthesize both drafts
     into one canonical plan (or revise the prior canonical if this is a
     revision round), get user approval, then send `topic="canonical"`.
   - `PlanClaudeFinalizePending` — enter Plan Mode, produce the final
     plan (incorporate Codex's review notes unless they conflict with the
     user's intent), get user approval, then send `topic="final"`. This
     locks the plan.
   - Any other phase — wait (loop).
7. After sending, loop back to step 1.

## Invariants — do not violate

- **Never** call `mcp__ironrace-memory__ironmem_collab_end`. It is
  reserved for the v2 coding phase.
- **Never** peek at Codex's draft before sending your own. The server
  enforces this in `recv`, but don't try to work around it.
- **Always** enter Plan Mode for `canonical` and `final` so the user can
  redirect before the hash commits.
- If the user interrupts with a question or correction, answer it inside
  Plan Mode and incorporate it into the next send.

## Unknown subcommand

If `$ARGUMENTS` does not start with `start` or `join`, tell the user:

```
Usage: /collab start <task>  |  /collab join <session_id>
```
