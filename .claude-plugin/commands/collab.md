---
description: Start or join an IronRace bounded planning session with Codex, auto-flowing into v2 coding if enabled. Covers v1 planning and v2 per-task debate → global review → PR handoff. Usage — /collab start <task>  |  /collab join <session_id>
argument-hint: start <task> | join <session_id>
---

<!-- DERIVED FROM docs/COLLAB.md — any protocol change must update BOTH files. -->

You are participating in the IronRace bounded collaboration protocol (v1 planning
+ v2 coding). Full spec: `docs/COLLAB.md`. The user has invoked `/collab` with
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
   plan locks (`PlanLocked`), the session automatically flows into the v2
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
     enter the v2 bridge (see "v2 bridge" section).
   - **v2 active** (`CodeImplementPending` .. `PrReadyPending`) →
     enter the v2 dispatch loop at the current phase (see "v2 dispatch loop").
   - **v2 terminal** (`CodingComplete` / `CodingFailed`) →
     report the status and exit.

## Dispatch Loop Structure

Both v1 and v2 share a common dispatch loop:

```text
loop:
  wait_my_turn(session_id, "claude", 60)
  status = collab_status(session_id)
  
  if session_ended or phase in terminal_set:
    exit and report to user
  
  if not is_my_turn:
    loop (continue polling)
  
  recv(session_id, "claude") → ack each message
  
  act on phase (send exactly one message per iteration)
  
  loop (restart polling)
```

Terminal sets:
- **v1**: `{PlanLocked}` (until `task_list` is sent)
- **v2**: `{CodingComplete, CodingFailed}`

Once `task_list` has been sent, `PlanLocked` is no longer terminal: the session
stays active and the terminal set flips to the v2 set above. The v2 bridge
(below) sends `task_list` and falls directly into the v2 dispatch loop, so the
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

## v2 Bridge: PlanLocked → CodeImplementPending

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
6. Fall through into the v2 dispatch loop.

## v2 Dispatch Loop (Phase → Action Table)

For every coding-active phase, execute this pre-send harness sequence before
building the payload:

**Pre-send Harness Sequence (v2 turns only):**
1. `collab_status(session_id)` → read `last_head_sha`.
2. `git cat-file -e <last_head_sha>^{commit}` — if the commit is missing,
   send `failure_report` with `coding_failure` field containing
   `"branch_drift: last_head_sha=<sha> not found in local repo"` and exit
   the loop (do not retry silently).
3. Run local gates **for phases that implement, finalize, or produce a local
   review** — specifically `CodeImplementPending`, `CodeFinalPending`,
   `CodeReviewLocalPending`, and `CodeReviewFinalPending`:
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - `cargo test --workspace`
4. On any gate failure, send `failure_report` with concrete error message
   (no silent retry). Include the exact error output.
5. Otherwise, proceed to the phase-specific action below.

| Phase | What to do (is_my_turn == true) |
|---|---|
| `CodeImplementPending` | **Run pre-send harness.** Write code for the current task. Commit and push. Call `collab_send` with `sender="claude"`, `topic="implement"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. |
| `CodeReviewPending` | Codex's turn. is_my_turn should be false. If `collab_status` confirms Claude is the owner, exit the loop and report the anomaly to the user; do not attempt a send. |
| `CodeVerdictPending` | **Run pre-send harness.** Read Codex's review. If local gates pass AND Codex's review raises no concrete actionable issue, send `verdict` with `"agree"`; otherwise send `verdict` with `"disagree_with_reasons"` and a terse justification. Payload: `{"head_sha":"<current HEAD>","verdict":"agree"|"disagree_with_reasons"}`. Escalate to the user in Plan Mode only if Codex flags something the gates cannot mechanically verify (architectural drift, plan-scope creep, security smell) or the choice is between two reasonable fixes. |
| `CodeDebatePending` | Codex's turn. is_my_turn should be false. If `collab_status` confirms Claude is the owner, exit the loop and report the anomaly. |
| `CodeFinalPending` | **Run pre-send harness (gates before finalize).** Apply fixes from Codex's comment. Re-run gates. Call `collab_send` with `sender="claude"`, `topic="final"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. |
| `CodeReviewLocalPending` | **Run pre-send harness.** Call `collab_send` with `sender="claude"`, `topic="review_local"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. Proactive review (`/ultrareview-local`) happens once, at `PrReadyPending` — not here. |
| `CodeReviewCodexPending` | Codex's turn. is_my_turn should be false. If `collab_status` confirms Claude is the owner, exit the loop and report the anomaly. |
| `CodeReviewVerdictPending` | **Run pre-send harness.** Read Codex's `review_global` message. If local gates still pass AND Codex's review_global raises no concrete actionable issue, send `verdict_global` with `"agree"`; otherwise send with `"disagree_with_reasons"` and a terse justification. Payload: `{"head_sha":"<current HEAD>","verdict":"agree"|"disagree_with_reasons"}`. Escalate to the user in Plan Mode if Codex flags something the gates cannot mechanically verify (architectural drift, plan-scope creep, security smell). |
| `CodeReviewDebatePending` | Codex's turn. is_my_turn should be false. If `collab_status` confirms Claude is the owner, exit the loop and report the anomaly. |
| `CodeReviewFinalPending` | **Run pre-send harness (gates before final).** Apply Codex's global comment fixes. Re-run gates. Call `collab_send` with `sender="claude"`, `topic="final_review"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. |
| `PrReadyPending` | **Run pre-send harness, then `/ultrareview-local` pre-PR validation.** If CRITICAL/HIGH surfaces: fix in place (commit + push), re-run gates, re-run `/ultrareview-local` until clean — the state machine stays at `PrReadyPending` throughout; only the `head_sha` you eventually send advances. If findings are out of scope for the locked plan, send `failure_report` with `coding_failure` describing the blocker. Once clean, **enter Plan Mode**: draft PR title (under 70 chars) and body (summary + test plan derived from task list + gate results + per-task acceptance checklist). Get user approval. Then `gh pr create --base <base_branch> --head <current branch> --title <approved title> --body <approved body>`. If `gh pr create` fails for any reason (auth, rate limit, conflict), send `failure_report` with `coding_failure: "pr_create_failed: <error>"` — no silent retry. On success, capture `pr_url` and call `collab_send` with `sender="claude"`, `topic="pr_opened"`, `content=<JSON {"head_sha":"<current HEAD>","pr_url":"<url>"}>`. Session advances to `CodingComplete`. Exit loop. |

After each send in v2, loop back to polling. The loop continues until
`phase in {CodingComplete, CodingFailed}` or `session_ended`.

## Invariants — do not violate

- **Never** call `mcp__ironmem__collab_end` during any active phase. Rejected in:
  - v1 active: `PlanParallelDrafts`, `PlanSynthesisPending`,
    `PlanCodexReviewPending`, `PlanClaudeFinalizePending`.
  - v2 active: `CodeImplementPending`, `CodeReviewPending`, `CodeVerdictPending`,
    `CodeDebatePending`, `CodeFinalPending`, `CodeReviewLocalPending`,
    `CodeReviewCodexPending`, `CodeReviewVerdictPending`, `CodeReviewDebatePending`,
    `CodeReviewFinalPending`, `PrReadyPending`.

  Only valid from `PlanLocked` pre-`task_list` (abandon plan), `CodingComplete`,
  or `CodingFailed`.
- **Never** peek at Codex's draft before sending your own during
  `PlanParallelDrafts`. The server enforces blind-draft in `recv`.
- **Plan Mode gates only at: v1 initial `draft` (from `/collab start`),
  v1 `final` (`PlanClaudeFinalizePending`), v2 `task_list` (bridge), and
  v2 `pr_opened` (`PrReadyPending`).** Every other turn — `canonical`,
  synthesis revisions, per-task `verdict`/`final`, global `verdict_global`/
  `final_review` — runs autonomously. Escalation exception: if a verdict turn
  flags a concern the gates cannot mechanically verify (architectural drift,
  plan-scope creep, security smell) or presents a genuine choice between two
  reasonable fixes, surface to the user in Plan Mode before sending.
- **Every v2 `collab_send` payload is JSON** per the matrix in `docs/COLLAB.md`.
  Never send prose payloads for v2 topics.
- **`head_sha` in every v2 payload must be the current `HEAD` AFTER any
  commit/push that preceded this turn.** The server records branch progress
  via `head_sha`.
- **Branch-drift carve-out:** `failure_report` may be sent by either agent at
  any time during a coding-active phase, independent of `current_owner`. It is
  the only topic that bypasses the owner check. A `coding_failure` prefixed
  `"branch_drift:"` is the canonical drift signal; do not suppress it.
- If the user interrupts with a question or correction during v1, answer it
  inside Plan Mode and incorporate it into the next send. During v2, gate at
  Plan Mode points only (`task_list`, `pr_opened`); all other turns are
  autonomous.

## Unknown subcommand

If `$ARGUMENTS` does not start with `start` or `join`, tell the user:

```
Usage: /collab start <task>  |  /collab join <session_id>
```
