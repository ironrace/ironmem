---
turn: review_local
tier: review
model: opus
topics: [review_local, failure_report, orphan_recovered]
preconditions: phase == CodeReviewLocalPending, current_owner == claude
---

# Collab worker — local review audit (CodeReviewLocalPending)

> ANTI-PUPPETEERING: You received only this template and `$SESSION_ID`. Discover
> all state yourself. Your final message MUST be the ≤3-line verdict only; never
> paste the review report.

## State discovery
1. `collab_status(session_id=$SESSION_ID)`; read `repo_path`, `branch`,
   `base_sha`, `last_head_sha`, and `pending_failure`. A non-null
   `pending_failure` means you are the **recovery owner** for an interrupted
   turn, not simply the next-in-line owner — see "Recoverable vs terminal
   failures" below before proceeding. A null `pending_failure` does not prove
   the previous turn finished: a turn killed hard never sends a
   `failure_report`, so the cleanliness check in Actions step 1 routes you
   onto that same recovery path.

## Recoverable vs terminal failures

The server classifies `failure_report`, not you — send an accurate
`coding_failure` prefix and let it decide. Six prefixes recover the turn
instead of ending the session: `git_commit_failed:`, `git_push_failed:`,
`sandbox_denied:`, `disk_full:`, `network_failed:`,
`codex_dispatch_failed:` (each needs real detail after the colon, e.g.
`git_commit_failed: index.lock EPERM`); everything else (including
`branch_drift:`/`subagent_failure:`) is terminal. If you are acting as
**recovery owner** — control was handed to you after Codex's recoverable
`failure_report`, you resumed via `collab_resume`, or you began a normal
turn and found a dirty worktree (step 1) —
preserve and inspect the working-tree diff before any fetch, checkout, or
reset; run this turn's gates yourself, commit + push, then send the
normal `review_local` (never a new `failure_report`). When you recovered
work, the review range head is your post-recovery `HEAD`, not
`last_head_sha`: substitute it for `<last_head_sha>` in the review-input
commands below, so you review `<base_sha>..<HEAD>` and the commits you just
recovered are inside the range you review, and send that same `HEAD`.
Reviewing the recorded range and sending a head beyond it makes the recovered
work the session head with nobody having read it.

If you reached this path from step 1's dirty-worktree check rather than from
a reported failure, a previous turn died without reporting it. After you
commit, record that once:
`collab_send(sender="claude", topic="orphan_recovered",
content=<JSON {"phase":"CodeReviewLocalPending","recovered_sha":"<HEAD>",
"detail":"<what you found>"}>)`. It records and returns — it does not advance
the phase, change owner, or spend a recovery attempt — so send it in addition
to, not instead of, the normal `review_local`.

## Actions
1. **Normal turns only — as recovery owner, skip the sync below and go
   straight to the gates at the end of this step.** Pre-send harness: work in
   `repo_path`; `git fetch`; `git cat-file -e <last_head_sha>^{commit}` (on
   miss → `failure_report` `branch_drift:...`); `git checkout <branch>` —
   this turn commits and pushes, so it must be on the session branch, not on
   whatever the previous turn left checked out. Immediately before resetting,
   require `git status --porcelain` to be empty regardless of
   `pending_failure`, and require `git rev-list <last_head_sha>..HEAD` to be
   empty as well: `--porcelain` says nothing about work that was committed
   but never pushed, and the reset discards it just the same. Either check
   failing means a prior turn died without reporting `pending_failure` (OOM,
   container kill, sandbox teardown), not that there is nothing to recover —
   do not run `git reset --hard`; instead preserve and inspect the diff on
   the recovery path above, which covers this case too. Only when the
   worktree is clean, `git reset --hard <last_head_sha>` (Codex pushed at
   `review_fix_global`).
   **Both normal and recovery turns** then run this turn's gates:
   `cargo fmt --check` + `clippy -D warnings`.
2. Prepare the normal review input before choosing depth. First attempt:
   ```bash
   ironmem review-diff --repo <repo_path> --base <base_sha> --head <last_head_sha>
   ```
   Inject its stdout **only on success**. On an error, unavailable feature, or
   nonbeneficial artifact, discard that output and use the exact raw fallback
   `git diff <base_sha>..<last_head_sha>`. Do not retain or inject a full raw
   diff when the artifact succeeds. Use the artifact's index to select source
   precisely when needed:
   ```bash
   ironmem review-diff --repo <repo_path> --base <base_sha> --head <last_head_sha> --expand-file <path> --hunk <ordinal>
   ```
   The artifact does not replace independent source inspection; read changed
   files and relevant callers directly before confirming findings.
3. Run the overlap-mode audit before choosing depth:
   - Find the preceding implementation head from collab status/event history
     (the `implementation_done.head_sha` immediately before `review_fix_global`).
   - If `last_head_sha` equals that implementation head, Codex made no fix commit:
     use `review_local=reduced`.
   - Else inspect `git diff --name-only <base_sha>..<last_head_sha>`. If every
     changed path is docs, prompts, scripts, config, metadata, or CI (no Rust
     source, Cargo manifests, lockfile, migrations, or runtime assets), use
     `review_local=reduced`.
   - If either check is uncertain, use `review_local=full`.
4. In `review_local=full`, run `/ultrareview-local --report-only` auditing
   Codex's commits + catching code-quality issues. The flag is required, not
   decorative: without it the command auto-fixes by default and dispatches
   Edit-capable agents into this session's working tree before you have
   verified a finding — and step 6 then cuts fix worktrees from, and step 7
   pushes, a tree already carrying those edits. Treat the review agents as
   read-only. Independently verify the synthesized findings and keep only
   confirmed CRITICAL/HIGH/MEDIUM issues.
5. In `review_local=reduced`, do a targeted read-only audit of the diff summary,
   changed files, and Codex commits for protocol drift, docs/config breakage,
   generated metadata inconsistencies, and security-sensitive configuration. Do
   not invoke `/ultrareview-local --report-only` unless the reduced audit finds
   a substantive uncertainty or a CRITICAL/HIGH/MEDIUM issue.
6. Group confirmed findings into non-overlapping fix clusters. For multiple
   independent clusters, create one temporary worktree per cluster on a unique
   throwaway branch from the same review head and dispatch fix subagents in
   parallel. Give each subagent exactly one cluster, tell it not to touch
   unrelated files, and have it return or commit only that cluster's edits. If
   findings overlap or touch the same fragile code path, fix that cluster
   sequentially instead of forcing unsafe parallelism.
7. Merge or cherry-pick the fix commits back onto the collab branch, resolve
   conflicts, then commit + push the integrated result.
8. Post-work gate: `cargo test --workspace` if fixes were committed or Rust/
   runtime files changed; otherwise use pushed-head proof and this turn's
   step-1 gate results as the gate evidence.
9. `collab_send(sender="claude", topic="review_local",
   content=<JSON {"head_sha":"<HEAD>"}>)`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: review_local sent (<n> fixes)
ref: head_sha:<HEAD>
blocker: <one line | none>
```
