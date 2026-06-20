---
turn: review_local
tier: review
model: opus
topics: [review_local, failure_report]
preconditions: phase == CodeReviewLocalPending, current_owner == claude
---

# Collab worker — local review audit (CodeReviewLocalPending)

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`. Discover
> all state yourself. Your final message MUST be the ≤3-line verdict only; never
> paste the review report.

## State discovery
1. `collab_status(session_id=$SESSION_ID)`; read `last_head_sha`.

## Actions
1. Pre-send harness: `git fetch`; `git cat-file -e <last_head_sha>^{commit}`
   (on miss → `failure_report` `branch_drift:...`); reset to `last_head_sha`
   (Codex pushed at `review_fix_global`); `cargo fmt --check` + `clippy -D warnings`.
2. Run `/ultrareview-local` auditing Codex's commits + catching code-quality
   issues. Treat the review agents as read-only. Independently verify the
   synthesized findings and keep only confirmed CRITICAL/HIGH/MEDIUM issues.
3. Group confirmed findings into non-overlapping fix clusters. For multiple
   independent clusters, create one temporary worktree per cluster on a unique
   throwaway branch from the same review head and dispatch fix subagents in
   parallel. Give each subagent exactly one cluster, tell it not to touch
   unrelated files, and have it return or commit only that cluster's edits. If
   findings overlap or touch the same fragile code path, fix that cluster
   sequentially instead of forcing unsafe parallelism.
4. Merge or cherry-pick the fix commits back onto the collab branch, resolve
   conflicts, then commit + push the integrated result.
5. Post-work gate: `cargo test --workspace`.
6. `collab_send(sender="claude", topic="review_local",
   content=<JSON {"head_sha":"<HEAD>"}>)`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: review_local sent (<n> fixes)
ref: head_sha:<HEAD>
blocker: <one line | none>
```
