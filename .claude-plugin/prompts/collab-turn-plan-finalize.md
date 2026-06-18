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
1. `collab_status(session_id=$SESSION_ID, verbose:true)`; read `canonical_plan`.
2. `collab_recv(session_id=$SESSION_ID, receiver="claude", auto_ack=true)` to
   read Codex's review notes.

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
