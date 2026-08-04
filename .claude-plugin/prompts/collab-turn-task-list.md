---
turn: task_list
tier: mechanical
model: sonnet
topics: [task_list]
preconditions: phase == PlanLocked, current_owner == pilot (claude or codex), passed in as SENDER
---

# Collab worker — submit task_list from approved final plan

> ANTI-PUPPETEERING: You received only this template, `$SESSION_ID`, and
> `$SENDER`. Discover all state yourself. Your final message MUST be the
> ≤3-line verdict only; never paste the plan markdown or the manifest.

## State discovery
1. `collab_status(session_id=$SESSION_ID)`; read `final_plan_ref`,
   `final_plan_hash`, `repo_path`, `branch`, and `current_owner`. Read current
   `HEAD` via git.
2. Verify `$SENDER` against `collab_status.current_owner` from step 1.
   `$SENDER` is authoritative for the send below and MUST NOT be
   substituted with your own identity. If `$SENDER` is absent, or does not
   equal `current_owner`, ABORT — do not send anything — and report the
   mismatch on the verdict's blocker line. One invariant applies:
   - **Normal case:** `current_owner == pilot`. `PlanLocked` is pre-coding and
     is not a `Phase::is_coding_active()` phase, so the server admits no
     `failure_report` and no recovery-owner substitution here — `$SENDER` is
     always the pilot, which under `pilot == "codex"` is `codex` even though
     this Claude worker performs the parse and the send.

## Actions
1. Read `plan_file_path` from `final_plan_ref.plan_file_path`. If it is missing
   or not repo-relative, do not send; return a blocker.
2. Verify the file at `plan_file_path` exists under `repo_path` and its
   SHA-256 equals both `final_plan_ref.hash` and `final_plan_hash`. Otherwise
   do not send and return a blocker. Do not fetch a plan drawer or recreate the
   file: the file plus hash is the approved-plan transport.
3. Parse the verified plan file's `### Task N:` headings into
   `{id, title, timebox_minutes, acceptance:[...]}`. The parser must verify:
   - heading count is at least 1
   - heading count is at most 10
   - IDs are contiguous `1..N`
   - every task has a `Timebox: <=20 minutes` line
   - no `timebox_minutes` value exceeds 20
   - every task has at least one acceptance criterion
   If there are more than 10 tasks, do not send `task_list`: return a blocker
   requiring the original issue to be split into independently executable child
   issues. Do not merge unrelated tasks or remove acceptance criteria to evade
   the limit.
4. Build the manifest `{plan_hash: final_plan_hash, base_sha:<HEAD>,
   head_sha:<HEAD>, plan_file_path:<path>, tasks:[...]}`. Add
   `execution_mode:"mechanical_direct"` only if the single-task eligibility rule
   in `docs/COLLAB.md` holds.
5. `collab_send(sender="$SENDER", topic="task_list", content=<JSON string>)`.
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
If a state-discovery check failed (the `$SENDER` mismatch/absence ABORT) or a
validation step above rejected the bridge, and you therefore sent nothing at
all, report `result: task_list not sent` with `ref: none` — never report a send
that did not happen. There is no `failure_report` form here: `PlanLocked` is
pre-coding, so the server rejects one.
