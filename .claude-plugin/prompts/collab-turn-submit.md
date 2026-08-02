---
turn: submit
tier: mechanical
model: sonnet
topics: [final, final_review, failure_report]
preconditions: a prior compose worker wrote $ARTIFACT_REF; for final the user approved, for final_review the orchestrator dispatches directly (no gate)
---

# Collab worker — submit-by-ref (post-gate sender)

> ANTI-PUPPETEERING: You received only this template, `$SESSION_ID`, `$TOPIC`,
> and `$ARTIFACT_REF`. Read the approved artifact by ref and send it. Do NOT
> re-author or editorialize. Your final message MUST be the ≤3-line verdict
> only.

## Recoverable vs terminal failures

The server classifies `failure_report`, not you — an accurate
`coding_failure` prefix is all that's needed. Six prefixes recover the turn
instead of ending the session: `git_commit_failed:`, `git_push_failed:`,
`sandbox_denied:`, `disk_full:`, `network_failed:`,
`codex_dispatch_failed:` (each needs real detail after the colon, e.g.
`git_commit_failed: index.lock EPERM`); everything else (including
`branch_drift:`/`subagent_failure:`, and both of THIS file's own failure
prefixes below — `pr_create_failed:`/`approved_artifact_unfetchable:` — is
terminal today). If `collab_status.pending_failure` is non-null, you are the
**recovery owner** for an interrupted turn: this changes nothing about the
Actions below — sending `$TOPIC` (e.g. `final_review`) via the normal
`collab_send` call below already IS the correct recovery-completion step
(never send a NEW `failure_report` just because you were not the original
`current_owner`).

## State discovery
1. `collab_status(session_id=$SESSION_ID)` to confirm phase/owner and read
   `repo_path`, `branch`, `base_sha`, and `pending_failure`.
2. Fetch the artifact named by `$ARTIFACT_REF`:
   - **drawer id** → `mcp__ironmem__get_drawer(id=$ARTIFACT_REF)` (deterministic
     read-by-id; do NOT use `search`, which is semantic and will not reliably
     return a freshly-staged drawer). If the response is `found:false`, treat the
     artifact as unfetchable and follow the failure path below.
   - **file path** → read the file.
   - Drawers are immutable (append-only); the `$ARTIFACT_REF` content cannot
     change, so no hash recompute is needed — the ref is the integrity anchor.
     (For `final` the ref is the user-approved drawer; for `final_review` the
     orchestrator dispatched it directly with no gate.)

## Actions
- If `$TOPIC == final_review`: parse the artifact JSON as
  `{"title":"...","body":"..."}`; resolve the base branch as the repository's
  **integration branch** — the remote default from
  `git symbolic-ref refs/remotes/origin/HEAD`, else the first of `origin/main`,
  `origin/master`, `origin/trunk` that **exists**. Containment of `base_sha` is
  a signal, never a requirement: a collab branch is routinely cut from a local
  commit that never landed on the remote default, so a base branch that does
  not contain `base_sha` is normal and MUST NOT fail this turn. Only a base
  that cannot be resolved at all is a resolution failure. When the resolved
  base does not contain `base_sha`, the branch carries commits that predate the
  reviewed range and were therefore covered by **neither** review pass — list
  them (`git log --oneline <base_branch>..<base_sha>`) in a short
  `## Unreviewed commits in this PR` section appended to the artifact body, so
  the human reviewer is told which commits no agent inspected. Then run
  `gh pr create --base <base_branch>
  --head $BRANCH --title <title> --body <body>`; capture `pr_url`;
  `collab_send(sender="claude", topic="final_review",
  content=<JSON {"head_sha":"<HEAD>","pr_url":"<url>"}>)`. On `gh` failure or
  base-branch resolution failure, send `failure_report`
  `content=<JSON {"coding_failure":"pr_create_failed: <error>"}>` (no silent
  retry).
- Otherwise (`final`): `collab_send(sender="claude", topic="final",
  content=<artifact body, JSON string {"plan":"<approved markdown>"}>)`.

### Failure path (artifact unfetchable, e.g. drawer missing)
Do NOT send the protocol topic. The valid recovery depends on the phase:
- `$TOPIC == final_review` (phase CodeReviewFinalPending is coding-active): a
  `failure_report` send IS valid — `collab_send(sender="claude",
  topic="failure_report", content=<JSON {"coding_failure":
  "approved_artifact_unfetchable:<$ARTIFACT_REF>"}>)` and stop.
- `$TOPIC == final` (v1 planning phase): the state machine
  REJECTS `failure_report` in these phases. Do NOT send anything — ABORT and
  describe the problem on the verdict's blocker line (e.g.
  "artifact unfetchable `<$ARTIFACT_REF>`; cannot send final").

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: $TOPIC sent
ref: <pr_url | none>
blocker: <one line | none>
```
