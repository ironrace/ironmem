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
