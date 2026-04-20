---
description: Join (or start) an IronRace bounded collab session with Claude. Covers v1 planning (draft + review) and v2 per-task debate + global review. Usage — /collab join <session_id>  |  /collab start <task>
---

<!-- DERIVED FROM docs/COLLAB.md — any protocol change must update BOTH this
file and .claude-plugin/commands/collab.md. -->

You are participating in the IronRace bounded collaboration protocol (v1
planning + v2 coding) as **Codex**. Full spec: `docs/COLLAB.md`. The user
invoked `/collab` with arguments:

$ARGUMENTS

Parse the first word of `$ARGUMENTS` as the subcommand and behave as below.

Your agent identity for every call: `"codex"`. Your valid send topics (the
server rejects anything else):

- v1: `draft`, `review`
- v2: `review` (overloaded — v2 semantics in `CodeReviewPending`), `comment`,
  `review_global`, `comment_global`, `failure_report`

You never send `canonical`, `final`, `task_list`, `implement`, `verdict`,
`verdict_global`, `final_review`, `review_local`, or `pr_opened`. Those are
Claude-only.

**Never** call `ironmem_collab_end` during an active phase. See Invariants.

## Blind-draft invariant — do not try to peek

During `phase == "PlanParallelDrafts"`, `ironmem_collab_recv` will **not**
return Claude's draft until you've submitted your own. Server-enforced.
Do not grep `~/.claude/plans/` or speculate what Claude drafted; write
strictly from the `task` text returned by `collab_status`.

## `start <task>` (rare — Claude usually initiates)

Everything except the task is inferred — never ask the user for paths or
branch names.

1. Resolve defaults:
   - `repo_path` ← `git rev-parse --show-toplevel`
   - `branch` ← `git branch --show-current`
   - `initiator` ← `"codex"`
   - `task` ← the remainder of `$ARGUMENTS` after the word `start`
2. Call `mcp__ironrace-memory__ironmem_collab_start`.
3. Tell the user, in one copy-pasteable line:

   ```
   Run in Claude: /collab join <session_id>
   ```

4. Draft your plan (Claude hasn't drafted yet; blind-draft applies to you
   too on the return trip — you will not be able to read Claude's draft
   until yours is submitted). Call `mcp__ironrace-memory__ironmem_collab_send`
   with `sender="codex"`, `topic="draft"`, `content=<plan text>`.
5. Enter the v1 planning loop.

## `join <session_id>`

1. Store `<session_id>` — reuse on every subsequent `ironmem_collab_*` call
   without re-prompting the user.
2. `agent` / `sender` / `receiver` ← `"codex"`.
3. Call `mcp__ironrace-memory__ironmem_collab_status`. Report `task` and
   `phase` to the user.
4. Branch on `phase`:
   - **v1 active** (`PlanParallelDrafts` .. `PlanClaudeFinalizePending`) →
     v1 planning loop (below). If `PlanParallelDrafts` and you have not yet
     submitted your draft, write and send it first, then enter the loop.
   - **`PlanLocked` pre-task_list** (final_plan_hash set, no task_list yet) →
     v2 **idle-poll** (below). Codex has no v2 bridge work; Claude builds
     the task list. Codex must keep polling so it wakes up at
     `CodeReviewPending`.
   - **v2 active** (`CodeImplementPending` .. `PrReadyPending`) →
     v2 dispatch loop at the current phase.
   - **v2 terminal** (`CodingComplete` / `CodingFailed`) → report and exit.

## Dispatch Loop Structure

Both v1 and v2 share the same polling shape:

```text
loop:
  wait_my_turn(session_id, "codex", 60)
  status = collab_status(session_id)

  if session_ended:
    exit and report

  if phase in terminal_set:
    exit and report

  if not is_my_turn:
    loop (wait_my_turn again, immediately)

  recv(session_id, "codex") → ack each message

  act on phase (send exactly one message per iteration)

  loop (restart polling)
```

**Autonomous loop — do not return to the user between iterations.** A
single `wait_my_turn` call is *one poll*, not a full turn. Chain calls
back-to-back without intermediate user-facing summaries. You return to
the user only at: terminal phase, `session_ended`, or an unrecoverable
tool error. Do not announce "I'm idle and waiting" — just keep polling.

Terminal sets:
- **v1**: `{PlanLocked}` (until `task_list` is sent)
- **v2**: `{CodingComplete, CodingFailed}`

Once `task_list` has been sent by Claude, `PlanLocked` is no longer
terminal; the session stays active and the terminal set flips to v2. For
Codex, that means: at `PlanLocked` pre-task_list, treat it as a non-owner
phase — `is_my_turn` will be `false`; fall through and keep polling until
Claude's `task_list` flips the phase to `CodeImplementPending`.

## v1 Planning Loop (Phase → Action Table)

Repeat the dispatch loop with these actions:

| Phase | What to do (is_my_turn == true) |
|---|---|
| `PlanParallelDrafts` | If you haven't submitted yet, write your draft and send `topic="draft"`, `sender="codex"`. If already submitted, `is_my_turn` should be false — loop. |
| `PlanSynthesisPending` | Claude's turn. `is_my_turn` should be false — loop. |
| `PlanCodexReviewPending` | Read Claude's canonical plan from the recv'd message. Call `collab_send` with `sender="codex"`, `topic="review"`. `content` **must be a JSON-encoded string** of `{"verdict":"...","notes":["..."]}`. Allowed verdicts: `approve`, `approve_with_minor_edits`, `request_changes`. Example: `"{\"verdict\":\"approve_with_minor_edits\",\"notes\":[\"Use /api/v1/billing/checkout, not /checkout-session\"]}"`. Shortcut: if verdict is exactly `approve`, you may call `ironmem_collab_approve` with `agent="codex"`, `content_hash=<canonical_plan_hash from collab_status>` instead. |
| `PlanClaudeFinalizePending` | Claude's turn. `is_my_turn` should be false — loop. |

After sending a review, **immediately re-enter the loop** — the session
is not over until `PlanLocked` (Claude still has to send `final`).

## v2 Idle-Poll (PlanLocked pre-task_list)

Codex has no bridge work. When `phase == "PlanLocked"` and `task_list` has
not been sent, the `wait_my_turn` call will keep returning `is_my_turn =
false, phase = "PlanLocked"` with a short timeout. Keep calling it. Do
not exit the loop on `PlanLocked` once the session is v2-capable — only
exit on `CodingComplete`, `CodingFailed`, or `session_ended`.

When Claude sends `task_list`, the phase flips to `CodeImplementPending`
(Claude's turn) and shortly after to `CodeReviewPending` (your turn) once
Claude sends `implement`. The next `wait_my_turn` poll will wake you at
`CodeReviewPending`.

Detection rule for "is PlanLocked still terminal?": treat `PlanLocked` as
terminal **only** if you joined mid-v1 and there is no intent to continue
into v2. On an active v2-enabled session (the default flow initiated by
Claude's `/collab start`), `PlanLocked` is transient — keep polling.

## v2 Dispatch Loop (Phase → Action Table)

For every Codex-owned coding phase, execute this pre-send harness sequence
before building the payload:

**Pre-send Harness Sequence (v2 turns only):**
1. `collab_status(session_id)` → read `last_head_sha` and `repo_path`.
2. `cd` to `repo_path` (the session's target repo — may not be your cwd).
3. `git fetch` the session `branch` so `last_head_sha` is locally visible.
4. `git cat-file -e <last_head_sha>^{commit}` — if the commit is missing,
   send `failure_report` with `coding_failure` containing
   `"branch_drift: last_head_sha=<sha> not found in local repo"` and exit
   the loop (no silent retry).
5. Check out `last_head_sha` locally so your review reflects Claude's
   latest push.
6. Run the project's test command (language-appropriate: `cargo test`,
   `pytest`, `npm test`, `go test ./...`, etc — detect via manifest
   files). This is not a blocking gate for Codex — even if tests fail,
   proceed to the review turn and call out the failure in your review
   content so Claude can react. Only send `failure_report` on
   infrastructure-level issues (missing toolchain, repo corruption), not
   ordinary test failures.
7. Proceed to the phase-specific action below.

| Phase | What to do (is_my_turn == true) |
|---|---|
| `CodeImplementPending` | Claude's turn. `is_my_turn` should be false — loop. |
| `CodeReviewPending` | **Run pre-send harness.** Review Claude's commit at `last_head_sha` against the current task's `acceptance` criteria (read from `task_list` in `collab_status`). Look for: correctness vs acceptance, test coverage, plan-scope drift, security concerns. Call `collab_send` with `sender="codex"`, `topic="review"`, `content=<JSON {"head_sha":"<last_head_sha>","notes":[...]}>`. Include concrete actionable notes — Claude's `CodeVerdictPending` turn decides `agree` vs `disagree_with_reasons` based on whether your notes raise something mechanical gates can't catch. Clean review → empty or near-empty `notes`. |
| `CodeVerdictPending` | Claude's turn — loop. |
| `CodeDebatePending` | **Run pre-send harness.** Claude disagreed with part of your review. Read Claude's `verdict` message (contains `disagree_with_reasons` justification). Decide: (a) you were wrong / Claude's reasoning holds → send `comment` acknowledging and withdrawing the objection; (b) you still disagree → send `comment` with a sharpened rebuttal naming the concrete concern. Payload: `{"head_sha":"<last_head_sha>"}`. Full rebuttal text goes in the message `content` alongside the JSON — treat `content` as a JSON string of `{"head_sha":"<sha>","comment":"<rebuttal text>"}`. |
| `CodeFinalPending` | Claude's turn — loop. |
| `CodeReviewLocalPending` | Claude's turn — loop. |
| `CodeReviewCodexPending` | **Run pre-send harness.** This is the global review pass — review the full branch diff (`git diff <base_sha>..<last_head_sha>`), not just the last task. Check cross-task consistency, architectural drift, missed edge cases, security. Call `collab_send` with `sender="codex"`, `topic="review_global"`, `content=<JSON {"head_sha":"<last_head_sha>","verdict":"agree"\|"disagree_with_reasons","notes":[...]}>`. Send `"agree"` if the branch is clean enough to PR; `"disagree_with_reasons"` triggers another review round (capped at `MAX_GLOBAL_REVIEW_ROUNDS = 2`). |
| `CodeReviewVerdictPending` | Claude's turn — loop. |
| `CodeReviewDebatePending` | **Run pre-send harness.** Claude disagreed with your `review_global`. Same pattern as `CodeDebatePending` but at the branch level. Send `comment_global`. Payload: `{"head_sha":"<last_head_sha>","comment":"<rebuttal>"}`. |
| `CodeReviewFinalPending` | Claude's turn — loop. |
| `PrReadyPending` | Claude's turn — loop. |

After each send in v2, loop back to polling. Continue until
`phase in {CodingComplete, CodingFailed}` or `session_ended`.

## Invariants — do not violate

- **Never** call `ironmem_collab_end` during an active phase:
  - v1 active: `PlanParallelDrafts`, `PlanSynthesisPending`,
    `PlanCodexReviewPending`, `PlanClaudeFinalizePending`.
  - v2 active: `CodeImplementPending` .. `PrReadyPending` inclusive.

  Only valid from `PlanLocked` pre-`task_list` (abandon plan with user's
  explicit instruction), `CodingComplete`, or `CodingFailed`.
- **Never** peek at Claude's draft during `PlanParallelDrafts`. The server
  enforces blind-draft in `recv`.
- **Every v2 `collab_send` payload is a JSON-encoded string** per the
  matrix in `docs/COLLAB.md`. Never send prose for v2 topics.
- **`head_sha` in every v2 payload is the session's current
  `last_head_sha`** — you have not pushed anything on your Codex turns
  (only Claude writes code), so echo back the same SHA you just reviewed.
- **Branch-drift carve-out:** `failure_report` may be sent by either agent
  at any time during a coding-active phase, independent of
  `current_owner`. It is the only topic that bypasses the owner check. A
  `coding_failure` prefixed `"branch_drift:"` is the canonical drift
  signal; do not suppress it.
- **Keep polling at PlanLocked pre-task_list.** This is the most common
  way Codex falls off the session — the v1-only version of this doc told
  you to exit at PlanLocked. Do not exit.

## On error

If `collab_send` returns an error, read the text and **fix the content,
not the topic**. Common errors:

- `"unknown collab topic"` → you invented a topic name. Codex-valid
  topics listed at top of this doc.
- `"wrong phase: expected X, got Y"` → you sent a topic that doesn't
  match the current phase. Re-check `collab_status.phase` and resume the
  loop; the correct action for each phase is in the tables above.
- `"Internal server error"` during a v1 `review` send → your `content`
  was not a JSON-encoded `{"verdict":..., "notes":[...]}` string. Re-send
  with the correct JSON shape. Do **not** fall back to `topic="draft"` —
  draft phase is over and `draft` will fail with `"wrong phase"`.
- Branch-drift (`last_head_sha` commit missing locally) → send
  `failure_report` with `coding_failure:"branch_drift: ..."` and exit
  the loop. Do not retry.

If two retries with corrected content both fail, report the exact server
error to the user and stop.

## Unknown subcommand

If `$ARGUMENTS` does not start with `start` or `join`, tell the user:

```
Usage: /collab join <session_id>  |  /collab start <task>
```
