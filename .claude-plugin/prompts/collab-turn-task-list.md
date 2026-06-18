---
turn: task_list
tier: mechanical
model: sonnet
topics: [task_list]
preconditions: phase == PlanLocked, current_owner == claude
---

# Collab worker — submit task_list from approved final plan

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`.
> Discover all state yourself. Your final message MUST be the ≤3-line verdict
> only; never paste the plan markdown or the manifest.

## State discovery
1. `collab_status(session_id=$SESSION_ID, verbose:true)`; read `final_plan`,
   `final_plan_hash`, `repo_path`, and `branch`. Read current `HEAD` via git.

## Actions
1. Extract `plan_file_path` from the leading markdown comment:
   `<!-- plan_file_path: <repo-relative path> -->`.
   If it is missing or not repo-relative, do not send; return a blocker.
2. Verify the file at `plan_file_path` exists under `repo_path`. If it is
   missing, recreate it from the exact `final_plan` body and create parent
   directories as needed. If it exists, its content must be byte-identical to
   `final_plan`; otherwise do not send and return a blocker.
3. Parse each `### Task N:` heading into
   `{id, title, timebox_minutes, acceptance:[...]}`. The parser must verify:
   - heading count is at least 1
   - IDs are contiguous `1..N`
   - every task has a `Timebox: <=20 minutes` line
   - no `timebox_minutes` value exceeds 20
   - every task has at least one acceptance criterion
4. Build the manifest `{plan_hash: final_plan_hash, base_sha:<HEAD>,
   head_sha:<HEAD>, plan_file_path:<path>, tasks:[...]}`. Add
   `execution_mode:"mechanical_direct"` only if the single-task eligibility rule
   in `docs/COLLAB.md` holds.
5. `collab_send(sender="claude", topic="task_list", content=<JSON string>)`.
   On any validation failure in this PlanLocked phase, do not send
   `failure_report`; PlanLocked is pre-coding, so the server rejects it before
   coding starts. Return the concrete problem on the `blocker:` line instead.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: task_list sent (<n> tasks)
ref: <plan_file_path>
blocker: <one line | none>
```
