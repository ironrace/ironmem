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
2. Call `mcp___collab_start` with those four fields.
3. Tell the user, in a single line they can copy-paste into Codex's terminal:

   ```
   Run in Codex: /collab join <session_id>
   ```

4. Enter Plan Mode and draft your first plan for `<task>` — the draft is
   yours alone, Codex cannot see it. When you have the user's approval in
   Plan Mode, call `mcp___collab_send` with
   `sender="claude"`, `topic="draft"`, `content=<the plan text>`.
5. After the draft is sent, begin the autonomous planning loop (see below).

## `join <session_id>`

1. Store `<session_id>` as the current collab session — reuse it on every
   subsequent `collab_*` call without re-prompting the user.
2. `agent` / `sender` / `receiver` ← `"claude"` (still Claude's terminal;
   in Codex's terminal this would be `"codex"`, handled by the Codex side).
3. Call `mcp___collab_status` to read `task`,
   `phase`, and `current_owner`. Report the task to the user.
4. Enter the autonomous planning loop.

## Autonomous planning loop (both start and join)

**Do not return to the user between iterations.** A single
`wait_my_turn` call is one poll, not a full iteration. Chain calls
back-to-back. The only times you return to the user are:

- `phase == "PlanClaudeFinalizePending"` (user approval gate via Plan Mode)
- `phase == "PlanLocked"` (report the locked plan)
- `session_ended == true`
- Unrecoverable tool error

Everything else is internal loop state — no "waiting on Codex" status
messages, no summaries. Just keep polling.

Repeat:

1. `mcp___collab_wait_my_turn` with
   `agent="claude"`, `timeout_secs=60`. Server-side long-poll.
2. If `session_ended` or `phase == "PlanLocked"`, exit and report.
3. If `is_my_turn == false`, **call `wait_my_turn` again immediately**
   — do not pause, do not report.
4. **`is_my_turn == true` → STOP POLLING.** Do not call `wait_my_turn`
   again until step 8. The next action is dictated by `phase`, not by
   another poll. If you catch yourself about to call `wait_my_turn` a
   second time in the same iteration, you have a bug — fall through
   to step 5.
5. `mcp___collab_status` → read `phase`,
   `current_owner`, `review_round`.
6. `mcp___collab_recv` with `receiver="claude"`.
   Ack each message via `mcp___collab_ack`. An
   empty `messages` array is fine — it means you already acked
   everything on a prior iteration. Do **not** re-poll because the
   queue is empty; proceed to step 7.
7. Act based on `phase`. **You MUST send a message this iteration.**
   If the table below says "loop," that means it is *not* your turn
   and is_my_turn should have been false at step 3 — re-verify with
   `collab_status`; don't silently re-poll.

   | Phase | What to do (is_my_turn == true) |
   |---|---|
   | `PlanParallelDrafts` | Your draft is already sent from the `start` branch above. is_my_turn should be false here — if true, check `collab_status` and report the anomaly. |
   | `PlanSynthesisPending` | **Do not ask the user.** Merge both drafts (or revise prior canonical on revision rounds) into a canonical plan. Call `collab_send` with `sender="claude"`, `topic="canonical"`, `content=<plan text>`. |
   | `PlanCodexReviewPending` | Codex's turn. is_my_turn should be false — if true, it's stale state. `collab_status` and re-check. |
   | `PlanClaudeFinalizePending` | **This is the only user-approval gate.** Enter Plan Mode. Produce the final plan, incorporating Codex's review notes unless they conflict with user intent. Get user approval. Call `collab_send` with `sender="claude"`, `topic="final"`, `content=<JSON string of {"plan":"<full text>"}>`. |

   Rationale: the user only wants to be interrupted once — when the
   plan is about to lock. Everything before that (drafts, synthesis,
   revisions) runs autonomously. The final is gated because that's the
   commit point: after it lands, `PlanLocked` is terminal.

8. After sending, loop back to step 1. **Never** call `wait_my_turn`
   more than once per iteration; if you find yourself making repeated
   `wait_my_turn` calls while `current_owner == "claude"`, you are
   stuck — break out, call `collab_status`, and act on `phase`.

## Invariants — do not violate

- **Never** call `mcp___collab_end`. It is
  reserved for the v2 coding phase.
- **Never** peek at Codex's draft before sending your own. The server
  enforces this in `recv`, but don't try to work around it.
- **Only** enter Plan Mode for `final` (canonical runs autonomously —
  no user gate). See the phase table.
- If the user interrupts with a question or correction, answer it inside
  Plan Mode and incorporate it into the next send.

## Unknown subcommand

If `$ARGUMENTS` does not start with `start` or `join`, tell the user:

```
Usage: /collab start <task>  |  /collab join <session_id>
```
