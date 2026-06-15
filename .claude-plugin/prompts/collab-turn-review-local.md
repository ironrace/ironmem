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
   issues. Fix CRITICAL/HIGH/MEDIUM inline; commit + push.
3. Post-work gate: `cargo test --workspace`.
4. `collab_send(sender="claude", topic="review_local",
   content=<JSON {"head_sha":"<HEAD>"}>)`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: review_local sent (<n> fixes)
ref: head_sha:<HEAD>
blocker: <one line | none>
```
