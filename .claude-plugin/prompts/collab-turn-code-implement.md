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
1. `collab_status(session_id=$SESSION_ID, verbose:true)`; read `plan_file_path`,
   `task_list`, `implementer`.
2. Search `wing="ironrace-memory" room="collab-checkpoints"` for `$SESSION_ID`;
   resume at the first unfinished task; scan the diff vs acceptance criteria.

## Load area maps first

Before touching any code in a task, call:
```
code_map_load(repo=<repo_path>, area=<touched_area>, turn_id=<turn_id>)
```
Interpret the response status and act accordingly:

- **`Fresh`** — the map is current. Use it as a WHERE-to-look pointer when
  deciding which files to open during exploration. Do NOT skip reading the
  actual files; the map tells you where to look, not what the code says.
- **`Stale`** — the map exists but `changed_files` have been modified since
  it was built. Re-read only the files listed in `changed_files`, then call
  `code_map_write` to refresh the map before proceeding.
- **`RescoutRequired` or absent** — no map exists for this area. Scout the
  area (read relevant files, trace call paths, identify key entry points),
  then call `code_map_write` to persist the map for future turns.

**Re-verify caveat:** Maps are WHERE-to-look pointers, not authoritative
documentation. Before relying on any load-bearing detail — function
signatures, type invariants, call-site counts — re-verify it against the
actual source code. Never trust a map entry alone for contract-level claims.

## Actions
1. Invoke `Skill('subagent-driven-development')` on `plan_file_path`. Auto-proceed
   between tasks (no per-task user gate). Write started/completed/blocked/
   batch_complete checkpoints per the `docs/COLLAB.md` checkpoint rule. STOP
   before `finishing-a-development-branch` (no PR here).
2. Verify no PR opened behind your back:
   `gh pr list --head $BRANCH --json number --jq 'length'` must be `0`.
3. Run gates: `cargo fmt --all -- --check`,
   `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
   `cargo test --workspace`.
4. On green: `collab_send(sender="claude", topic="implementation_done",
   content=<JSON {"head_sha":"<HEAD>"}>)`. On failure/overrun:
   `collab_send(topic="failure_report", content=<JSON {"coding_failure":"..."}>)`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: <implementation_done sent | failure_report sent>
ref: head_sha:<HEAD>
blocker: <one line | none>
```
