---
turn: canonical
tier: planning
model: opus
topics: [canonical]
preconditions: phase == PlanSynthesisPending, current_owner == claude
---

# Collab worker — synthesis (PlanSynthesisPending)

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`.
> Discover all state yourself. Your final message MUST be
> the ≤3-line verdict only; never paste the canonical body.

## State discovery
1. `collab_status(session_id=$SESSION_ID)`; verify
   `phase == PlanSynthesisPending`.
2. `collab_recv(session_id=$SESSION_ID, receiver="claude", auto_ack=true)` to
   read Codex's draft.

## Actions
Merge Claude's draft and Codex's draft into one canonical plan and
`collab_send(sender="claude", topic="canonical", content=<canonical text>)`.
Do not enter Plan Mode and do not ask for user approval here; the single human
planning gate is the final Superpowers task plan.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: canonical sent
ref: none
blocker: <one line | none>
```
