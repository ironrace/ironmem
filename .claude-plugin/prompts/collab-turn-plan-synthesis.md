---
turn: canonical
tier: planning
model: opus
topics: [canonical]
preconditions: phase == PlanSynthesisPending, current_owner == claude
---

# Collab worker — synthesis (PlanSynthesisPending)

> ANTI-PUPPETEERING: You received only this template, `$SESSION_ID`, and `$MODE`
> (`compose` or `send`). Discover all state yourself. Your final message MUST be
> the ≤3-line verdict only; never paste the canonical body.

## State discovery
1. `collab_status(session_id=$SESSION_ID)`; read `review_round`. On revision
   rounds also pass `verbose:true` and read `canonical_plan` (prior canonical).
2. `collab_recv(session_id=$SESSION_ID, receiver="claude", auto_ack=true)` to
   read Codex's draft (round 0) or Codex's `review` notes (revision rounds).

## Actions
- If `review_round == 0` AND `$MODE == compose`: merge both drafts into the
  canonical; `add_drawer(wing="ironrace-memory", room="collab-drafts",
  content=<canonical text>)`; return its `drawer_id` as the ref. Do NOT send.
- If `review_round >= 1` (revision) OR `$MODE == send`: produce/refresh the
  canonical and `collab_send(sender="claude", topic="canonical",
  content=<canonical text>)`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: <canonical composed | canonical sent>
ref: <drawer_id hash:<h> | none>
blocker: <one line | none>
```
