---
turn: final
tier: planning
model: opus
topics: [final]
preconditions: phase == PlanClaudeFinalizePending, current_owner == claude
---

# Collab worker — finalize (PlanClaudeFinalizePending)

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`. Discover
> all state yourself. Your final message MUST be the ≤3-line verdict only; never
> paste the final-plan body.

## State discovery
1. `collab_status(session_id=$SESSION_ID, verbose:true)`; read `canonical_plan`.
2. `collab_recv(session_id=$SESSION_ID, receiver="claude", auto_ack=true)` to
   read Codex's review notes.

## Actions
1. Produce the final plan, incorporating Codex's notes unless they conflict with
   user intent.
2. `add_drawer(wing="ironrace-memory", room="collab-drafts",
   content=<JSON string {"plan":"<full text>"}>)`; return its `drawer_id`.
   Compute the SHA-256 of the artifact body you stored and return it as the
   `hash` in the verdict `ref:` line. Do NOT send — the orchestrator gates,
   then dispatches `collab-turn-submit.md` with `$TOPIC=final`,
   `$ARTIFACT_REF=<drawer_id>`, and `$ARTIFACT_HASH=<that hash>`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: final composed
ref: <drawer_id hash:<h>>
blocker: <one line | none>
```
