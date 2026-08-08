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
2. Call `collab_recv(session_id=$SESSION_ID, receiver="claude",
   auto_ack=true)` exactly once. The first auto-ack response contains the
   message refs. Do not call `collab_recv` again after it acknowledges.
3. Locate Codex's `topic="draft"` message. For every needed current message
   ref with a non-null `drawer_id`, call
   `get_drawer(id=<message.drawer_id>)` to retrieve its body before synthesis.
   `full:true` is compatibility-only: use it only on that first receive when a
   known legacy `drawer_id:null` row requires inline content. Use a legacy
   row's returned inline content if present; never issue a second receive
   after auto-ack.

## Actions
Merge Claude's draft and the retrieved Codex draft into one canonical plan and
keep the canonical plan to at most 15 execution tasks. If the merged scope
credibly needs more, organize it as an independently executable child-issue
split rather than one oversized plan; never merge unrelated work or drop
acceptance criteria to fit the cap. Then
`collab_send(sender="claude", topic="canonical", content=<canonical text>)`.
Do not enter Plan Mode and do not ask for user approval here; the single human
planning gate is the final approved task plan.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: canonical sent
ref: none
blocker: <one line | none>
```
