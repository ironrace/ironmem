---
turn: implementation_done
tier: mechanical
model: sonnet
topics: [implementation_done, failure_report]
preconditions: phase == CodeImplementPending, current_owner == claude, implementer == claude
---

# Collab worker — Claude batch implementation (CodeImplementPending)

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`. Discover
> all state yourself. Your final message MUST be the ≤3-line verdict only; never
> paste diffs, task notes, or self-critique.

## State discovery
1. `collab_status(session_id=$SESSION_ID)`; read `plan_file_path`,
   `task_list_ref`, `tasks_count`, `implementer`, `base_sha`,
   `pending_failure`. `base_sha` is fixed at plan-lock time and never
   advances, so it is the plan-level base `iron-build`'s final
   whole-implementation review needs — including on a mid-plan resume, where
   the first task's `BASE_SHA` was never recorded locally. A
   non-null `pending_failure` means you are the **recovery owner** for an
   interrupted turn, not simply the next-in-line owner — see "Recoverable vs
   terminal failures" below before proceeding.
2. Read the current checkpoint from `collab_status`'s `checkpoint` block. If
   it reports `diverged: true` or `diverged: null` (unreadable is not "no
   divergence"), do NOT resume on that progress claim: inspect first with
   `collab_checkpoint(session_id=$SESSION_ID, agent="claude",
   inspect_divergence=true)`, then file an accurate checkpoint or escalate
   for an operator-attested backfill per `docs/COLLAB.md`. Otherwise resume
   at the first unfinished task; scan the diff vs acceptance criteria.

## Load area maps first

Before touching any code in a task, call:
```
code_map_status(repo=<repo_path>, area=<touched_area>)
```
Interpret the response status and act accordingly:

- **`Fresh`** — the map is current. If you need the map body as a WHERE-to-look
  pointer, call `code_map_load(repo=<repo_path>, area=<touched_area>,
  turn_id=<turn_id>)`; otherwise open the relevant source files directly. Do
  NOT skip reading the actual files; the map tells you where to look, not what
  the code says.
- **`Stale`** — the map exists but source files changed. Do not call
  `code_map_load` for the stale body; re-scout the area from source, then call
  `code_map_write` to refresh the map before proceeding.
- **`RescoutRequired` or absent** — no map exists for this area. Scout the
  area (read relevant files, trace call paths, identify key entry points),
  then call `code_map_write` to persist the map for future turns.

**Re-verify caveat:** Maps are WHERE-to-look pointers, not authoritative
documentation. Before relying on any load-bearing detail — function
signatures, type invariants, call-site counts — re-verify it against the
actual source code. Never trust a map entry alone for contract-level claims.

## Recoverable vs terminal failures

The server classifies `failure_report`, not you — send an accurate
`coding_failure` prefix and let it decide. Seven prefixes recover the turn
instead of ending the session: `git_commit_failed:`, `git_push_failed:`,
`sandbox_denied:`, `disk_full:`, `network_failed:`,
`codex_dispatch_failed:`, `checkpoint_drift:` (each needs real detail after
the colon, e.g. `git_commit_failed: index.lock EPERM`); everything else
(including `branch_drift:`/`subagent_failure:`) is terminal. If you are
acting as **recovery owner** — control was handed to you after Codex's recoverable
`failure_report`, or you resumed via `collab_resume` — inspect the
preserved diff, run this turn's gates yourself, commit + push, then send
the normal `implementation_done` (never a new `failure_report`).

## Actions
1. Invoke `Skill('iron-build')` on `plan_file_path`. Auto-proceed
   between tasks (no per-task user gate). Write started/completed/blocked/
   batch_complete checkpoints per the `docs/COLLAB.md` checkpoint rule with
   `collab_checkpoint(session_id=$SESSION_ID, agent="claude", ...)`, carrying
   the full cumulative `completed_task_ids` forward on every write since each
   write replaces the session's one current checkpoint row. STOP
   before `iron-build`'s *Finishing the Branch* step (no PR here).
2. Verify the local boundary invariant: the skill stopped after the final
   task's approval+commit, did not run the *Finishing the Branch* step,
   and did not report running a PR-producing command. Do not query GitHub by
   default. Run `gh pr list --head $BRANCH --json number --jq 'length'` only
   if the controller reports boundary uncertainty or the skill output mentions
   PR creation / the *Finishing the Branch* step; if it returns >=1, send
   `failure_report` with
   `coding_failure: "skill_overran_pr_boundary: <pr_number>"`.
3. Run gates: `cargo fmt --all -- --check`,
   `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
   `cargo test --workspace`.
4. On green: write the final `status: batch_complete` checkpoint at the
   current HEAD — `completed_task_ids` covering every task,
   `gates_result=passed`, `gates_sha=<HEAD>` — **before** sending. Without a
   matching `batch_complete` checkpoint at this exact `head_sha`,
   `implementation_done` is refused with a `checkpoint_drift:` error naming
   the exact remedy call. Then `collab_send(sender="claude",
   topic="implementation_done", content=<JSON {"head_sha":"<HEAD>"}>)`. On
   failure/overrun:
   `collab_send(topic="failure_report", content=<JSON {"coding_failure":"..."}>)`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: <implementation_done sent | failure_report sent>
ref: head_sha:<HEAD>
blocker: <one line | none>
```
