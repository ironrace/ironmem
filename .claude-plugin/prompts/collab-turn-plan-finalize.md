---
turn: final
tier: planning
model: opus
topics: [final]
preconditions: phase == PlanClaudeFinalizePending, current_owner == claude
---

# Collab worker — finalize into iron-build-compatible task markdown

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`. Discover
> all state yourself. Your final message MUST be the ≤3-line verdict only; never
> paste the final-plan body.

## State discovery
1. `collab_status(session_id=$SESSION_ID)`; verify
   `phase == PlanClaudeFinalizePending` and inspect `canonical_plan_ref`.
2. Call `collab_recv(session_id=$SESSION_ID, receiver="claude",
   auto_ack=true)` exactly once. The first auto-ack response contains the
   message refs. Do not call `collab_recv` again after it acknowledges.
3. Before finalization, dereference every needed current body: call
   `get_drawer(id=<canonical_plan_ref.drawer_id>)` for the canonical plan and
   `get_drawer(id=<message.drawer_id>)` for Codex's `topic="review"` message.
   `full:true` is compatibility-only: use it only on that first receive when a
   known legacy review row requires inline content. A legacy canonical plan
   without a drawer reference cannot be recovered through status; return a
   blocker. Never issue a second receive after auto-ack.

## Actions
1. Produce the final execution plan as an iron-build-compatible task markdown
   document, not as a prose plan that needs a second planning conversion later.
   Incorporate Codex's review notes unless they conflict with user intent.
2. Save the exact markdown to
   `docs/iron/plans/YYYY-MM-DD-<short-feature>.md`. The first non-blank
   line of the file must be:
   `<!-- plan_file_path: docs/iron/plans/YYYY-MM-DD-<short-feature>.md -->`
3. Use `### Task N: <title>` headings. Every task must include:
   - `Timebox: <=20 minutes`
   - at least one concrete acceptance criterion
   - the files/areas it is expected to touch
   If a task cannot credibly be completed in 20 minutes, split it before saving.
   The plan must contain at most 15 tasks. If it needs 16 or more, do not
   compose or submit a collab plan: return a blocker that names the required
   independently executable child-issue split. Never merge unrelated work or
   drop acceptance criteria merely to fit this budget.
4. Run a local structure check on the markdown you wrote:
   - heading count for `^### Task ` is at least 1
   - heading count is at most 15
   - task IDs are contiguous `1..N`
   - every task has `Timebox: <=20 minutes`
   - every task has at least one acceptance criterion
5. `add_drawer(wing="ironrace-memory", room="collab-drafts",
   content=<JSON string {"plan":"<exact markdown>"}>)`; return its `drawer_id`.
   Do NOT send — this turn is autonomous and there is NO human approval gate on
   it. Right after this turn the orchestrator dispatches
   `collab-turn-submit.md` with `$TOPIC=final` and `$ARTIFACT_REF=<drawer_id>`
   to send `final`, without asking a human. The single human planning gate is
   the dispatcher's and fires one phase later, at `PlanLocked`, before the
   `task_list` bridge is dispatched; the `{drawer_id, plan_file_path, ≤3-line
   summary}` you return here is what the dispatcher carries to that gate.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: final task plan composed (<n> tasks, file:<plan_file_path>)
ref: <drawer_id | none>
blocker: <one line | none>
```
