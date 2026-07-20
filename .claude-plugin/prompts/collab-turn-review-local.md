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

## Recoverable vs terminal failures

The server classifies `failure_report`, not you — send an accurate
`coding_failure` prefix and let it decide. Six prefixes recover the turn
instead of ending the session: `git_commit_failed:`, `git_push_failed:`,
`sandbox_denied:`, `disk_full:`, `network_failed:`,
`codex_dispatch_failed:` (each needs real detail after the colon, e.g.
`git_commit_failed: index.lock EPERM`); everything else (including
`branch_drift:`/`subagent_failure:`) is terminal. If you are acting as
**recovery owner** — control was handed to you after Codex's recoverable
`failure_report`, or you resumed via `collab_resume` — inspect the
preserved diff, run this turn's gates yourself, commit + push, then send
the normal `review_local` (never a new `failure_report`).

## Actions
1. Pre-send harness: `git fetch`; `git cat-file -e <last_head_sha>^{commit}`
   (on miss → `failure_report` `branch_drift:...`); reset to `last_head_sha`
   (Codex pushed at `review_fix_global`); `cargo fmt --check` + `clippy -D warnings`.
2. Run the overlap-mode audit before choosing depth:
   - Find the preceding implementation head from collab status/event history
     (the `implementation_done.head_sha` immediately before `review_fix_global`).
   - If `last_head_sha` equals that implementation head, Codex made no fix commit:
     use `review_local=reduced`.
   - Else inspect `git diff --name-only <base_sha>..<last_head_sha>`. If every
     changed path is docs, prompts, scripts, config, metadata, or CI (no Rust
     source, Cargo manifests, lockfile, migrations, or runtime assets), use
     `review_local=reduced`.
   - If either check is uncertain, use `review_local=full`.
3. In `review_local=full`, run `/ultrareview-local` auditing Codex's commits +
   catching code-quality issues. Treat the review agents as read-only.
   Independently verify the synthesized findings and keep only confirmed
   CRITICAL/HIGH/MEDIUM issues.
4. In `review_local=reduced`, do a targeted read-only audit of the diff summary,
   changed files, and Codex commits for protocol drift, docs/config breakage,
   generated metadata inconsistencies, and security-sensitive configuration. Do
   not invoke `/ultrareview-local` unless the reduced audit finds a substantive
   uncertainty or a CRITICAL/HIGH/MEDIUM issue.
5. Group confirmed findings into non-overlapping fix clusters. For multiple
   independent clusters, create one temporary worktree per cluster on a unique
   throwaway branch from the same review head and dispatch fix subagents in
   parallel. Give each subagent exactly one cluster, tell it not to touch
   unrelated files, and have it return or commit only that cluster's edits. If
   findings overlap or touch the same fragile code path, fix that cluster
   sequentially instead of forcing unsafe parallelism.
6. Merge or cherry-pick the fix commits back onto the collab branch, resolve
   conflicts, then commit + push the integrated result.
7. Post-work gate: `cargo test --workspace` if fixes were committed or Rust/
   runtime files changed; otherwise use pushed-head proof and the pre-send
   harness result as the gate evidence.
8. `collab_send(sender="claude", topic="review_local",
   content=<JSON {"head_sha":"<HEAD>"}>)`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: review_local sent (<n> fixes)
ref: head_sha:<HEAD>
blocker: <one line | none>
```
