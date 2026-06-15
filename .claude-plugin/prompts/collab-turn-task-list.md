---
turn: task_list
tier: planning
model: opus
topics: [task_list]
preconditions: phase == PlanLocked, current_owner == claude
---

# Collab worker — PlanLocked bridge (task_list)

> ANTI-PUPPETEERING: You received only this template, `$SESSION_ID`, and `$MODE`
> (`compose` or `submit`). Discover all state yourself. Your final message MUST
> be the ≤3-line verdict only; never paste the plan markdown or the manifest.

## State discovery
1. `collab_status(session_id=$SESSION_ID, verbose:true)`; read `final_plan`,
   `final_plan_hash`. Read current `HEAD` via git.

## Actions
- `$MODE == compose`: invoke `Skill('writing-plans')` on `final_plan` in
  produce-only mode (do NOT trigger its interactive "execute now?" handoff).
  It saves `docs/superpowers/plans/YYYY-MM-DD-<feature>.md`. Return that
  `plan_file_path` + content hash. Do NOT send.
- `$MODE == submit` (after approval): read `plan_file_path`; parse each
  `### Task N:` heading into `{id, title, acceptance:[...]}`; build the manifest
  `{plan_hash, base_sha:<HEAD>, head_sha:<HEAD>, plan_file_path, tasks:[...]}`
  (add `execution_mode:"mechanical_direct"` only if the single-task eligibility
  rule in `docs/COLLAB.md` holds); `collab_send(sender="claude",
  topic="task_list", content=<JSON string>)`. If zero tasks parse, send a
  `failure_report` instead.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: <plan composed | task_list sent (<n> tasks)>
ref: <plan_file_path hash:<h>>
blocker: <one line | none>
```
