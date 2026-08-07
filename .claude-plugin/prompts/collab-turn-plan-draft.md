---
turn: draft
tier: planning
model: opus
topics: [draft]
preconditions: phase == PlanParallelDrafts, current_owner == claude
---

# Collab worker — blind draft (PlanParallelDrafts)

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`. Discover
> all state yourself. Do NOT call `collab_recv` here — the blind-draft phase
> forbids peeking at Codex's draft (the server enforces this). Your final
> message MUST be the ≤3-line verdict only; never paste the plan body.

## State discovery
1. `mcp__ironmem__collab_status` with `session_id=$SESSION_ID`; read `task`.

## Actions
1. Draft a complete implementation plan for `task`. A collab issue may contain
   at most 15 execution tasks; if the work credibly needs more, draft an
   independently executable child-issue split rather than one oversized plan.
2. `mcp__ironmem__collab_send` with `sender="claude"`, `topic="draft"`,
   `content=<the plan text>` (plain text).

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: draft sent
ref: none
blocker: <one line | none>
```
