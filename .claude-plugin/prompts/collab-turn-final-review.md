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
1. `collab_status(session_id=$SESSION_ID)`; read `task_list_ref`,
   `last_head_sha`, `pending_failure`.
2. When composing the PR body, load task details by reference:
   `get_drawer(id=<task_list_ref.drawer_id>)` and verify its SHA-256 against
   `task_list_ref.hash`. If `task_list_ref.drawer_id` is null on a legacy
   session, return a blocker: `collab_status(include_task_list:true)` is
   reference-only and never returns task-list JSON.

## Recoverable vs terminal failures

The server classifies `failure_report`, not you — an accurate
`coding_failure` prefix is all that's needed. Six prefixes recover the turn
instead of ending the session: `git_commit_failed:`, `git_push_failed:`,
`sandbox_denied:`, `disk_full:`, `network_failed:`,
`codex_dispatch_failed:` (each needs real detail after the colon, e.g.
`git_commit_failed: index.lock EPERM`); everything else (including
`branch_drift:`/`subagent_failure:`) is terminal.

**This template neither sends `failure_report`/`final_review` nor runs
gates** — it only drafts the PR body and hands off to
`collab-turn-submit.md`, which does both. A non-null `pending_failure` on
entry (you are the recovery owner for an interrupted turn) does not change
what THIS template does: draft the PR body exactly as in step 2 of Actions
below, same as any other invocation. `collab-turn-submit.md` is where the
actual recovery-owner protocol (inspect the preserved diff, run this
phase's gates, commit + push, send the normal completion event, never a new
`failure_report`) applies — see that file.

## Actions
1. Pushed-head proof only (no reset and do NOT re-run gates): verify
   `git cat-file -e <last_head_sha>^{commit}`, clean worktree,
   `git rev-parse HEAD == <last_head_sha>`, and local HEAD matches the pushed
   upstream head (`@{u}`, or `refs/remotes/origin/<branch>` if no upstream is
   configured). If any proof check fails, do not run tests; return a blocker so
   the orchestrator can triage branch drift.
2. Draft the PR title (<70 chars) + body (summary + test plan from task_list +
   prior gate evidence / pushed-head proof). `add_drawer(wing="ironrace-memory", room="collab-drafts",
   content=<JSON string {"title":"<title>","body":"<body>"}>)`; return its
   `drawer_id`. Do NOT open the PR — the orchestrator dispatches
   `collab-turn-submit.md` **directly** (no user-approval gate at this phase)
   with `$TOPIC=final_review` and `$ARTIFACT_REF=<drawer_id>` to run a plain
   `gh pr create` (ready PR, no `--draft`) and send.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: pr body composed (title: <title>)
ref: <drawer_id | none>
blocker: <one line | none>
```
