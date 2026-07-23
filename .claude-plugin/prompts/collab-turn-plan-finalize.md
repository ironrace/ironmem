---
turn: final
tier: planning
model: opus
topics: [final]
preconditions: phase == PlanClaudeFinalizePending, current_owner == claude
---

# Collab worker — finalize into Superpowers task plan

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
   known legacy `drawer_id:null` row requires inline content. For a legacy
   canonical plan without a reference, use the status response's inline
   `canonical_plan`; for a legacy review row, use its returned inline content.
   Never issue a second receive after auto-ack.

## Actions
1. Produce the final execution plan as a Superpowers-compatible task markdown
   document, not as a prose plan that needs a second planning conversion later.
   Incorporate Codex's review notes unless they conflict with user intent.
2. Save the exact markdown to
   `docs/superpowers/plans/YYYY-MM-DD-<short-feature>.md`. The first non-blank
   line of the file must be:
   `<!-- plan_file_path: docs/superpowers/plans/YYYY-MM-DD-<short-feature>.md -->`
3. Use `### Task N: <title>` headings. Every task must include:
   - `Timebox: <=20 minutes`
   - at least one concrete acceptance criterion
   - the files/areas it is expected to touch
   If a task cannot credibly be completed in 20 minutes, split it before saving.
4. Run a local structure check on the markdown you wrote:
   - heading count for `^### Task ` is at least 1
   - task IDs are contiguous `1..N`
   - every task has `Timebox: <=20 minutes`
   - every task has at least one acceptance criterion
5. `add_drawer(wing="ironrace-memory", room="collab-drafts",
   content=<JSON string {"plan":"<exact markdown>"}>)`; return its `drawer_id`.
   Do NOT send — the orchestrator presents only the file path, drawer ref, and
   short summary for the single human planning approval, then dispatches
   `collab-turn-submit.md` with `$TOPIC=final` and `$ARTIFACT_REF=<drawer_id>`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: final task plan composed (<n> tasks, file:<plan_file_path>)
ref: <drawer_id | none>
blocker: <one line | none>
```
