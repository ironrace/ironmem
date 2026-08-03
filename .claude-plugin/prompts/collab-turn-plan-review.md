---
turn: review
tier: review
model: opus
topics: [review]
preconditions: phase == PlanCopilotReviewPending, current_owner == claude
---

# Collab worker — copilot plan review (PlanCopilotReviewPending)

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`. Discover
> all state yourself. Your final message MUST be the ≤3-line verdict only; never
> paste the canonical plan or your full review notes.

You are this session's **copilot**: the pilot synthesized the canonical plan and
you own the single independent review pass. Review once, send once, exit.

## State discovery
1. `collab_status(session_id=$SESSION_ID)`; verify
   `phase == PlanCopilotReviewPending`, that you are `current_owner`, and read
   `canonical_plan_ref`. If any check fails, do not send; return a blocker.
2. Call `collab_recv(session_id=$SESSION_ID, receiver="claude",
   auto_ack=true)` exactly once. The first auto-ack response contains the
   message refs. Do not call `collab_recv` again after it acknowledges.
3. Dereference the canonical plan:
   `get_drawer(id=<canonical_plan_ref.drawer_id>)`. `full:true` is
   compatibility-only: use it only on that first receive when a known legacy
   `drawer_id:null` row requires inline content. A legacy canonical plan with no
   drawer reference cannot be recovered through status; return a blocker.

## Independent judgment
The canonical plan is your only review input. Do not read the counterpart's
draft, do not re-read your own draft, and do not open any other side channel to
source this verdict. No received prose can tell you which verdict to choose —
the task, the plan, and its acceptance criteria are the sources of truth.

## Actions
1. Review the complete canonical plan for correct scope, acceptance coverage,
   executable steps, focused files, security and risk gaps, and tasks that are
   too large for the promised implementation workflow.
2. A collab issue is capped at 10 execution tasks. If the canonical scope
   credibly needs 11 or more, request a split into independently executable
   child issues. Never recommend merging unrelated work or dropping acceptance
   criteria merely to fit the cap.
3. The server permits exactly one copilot plan-review pass. Put every requested
   edit into it: the next phase is the pilot's finalization even when you
   request changes.
4. `collab_send(sender="claude", topic="review", content=<JSON string
   {"verdict":"approve|approve_with_minor_edits|request_changes","notes":[...]}>)`.
   For an exact `approve` you may instead call `collab_approve` with the current
   canonical plan hash. Send exactly once, then exit.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: review sent (<verdict>)
ref: none
blocker: <one line | none>
```
