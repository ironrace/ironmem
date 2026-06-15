---
turn: final_review
tier: review
model: opus
topics: [final_review]
preconditions: phase == CodeReviewFinalPending, current_owner == claude
---

# Collab worker — final review / PR-body compose (CodeReviewFinalPending)

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`. Discover
> all state yourself. Your final message MUST be the ≤3-line verdict only; never
> paste the PR artifact.

## State discovery
1. `collab_status(session_id=$SESSION_ID, verbose:true)`; read `task_list`,
   `last_head_sha`.

## Actions
1. Pre-send harness (no reset — Claude pushed at `review_local`): `git cat-file`
   check; re-run gates.
2. Draft the PR title (<70 chars) + body (summary + test plan from task_list +
   gate results). `add_drawer(wing="ironrace-memory", room="collab-drafts",
   content=<JSON string {"title":"<title>","body":"<body>"}>)`; return its
   `drawer_id`. Do NOT open the PR — the orchestrator gates, then dispatches
   `collab-turn-submit.md` `$MODE=send` `$TOPIC=final_review` to run
   `gh pr create` and send.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: pr body composed (title: <title>)
ref: <drawer_id hash:<h>>
blocker: <one line | none>
```
