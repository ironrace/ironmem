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

## State discovery
1. `collab_status(session_id=$SESSION_ID)` to confirm phase/owner and read
   `repo_path`, `branch`, and `base_sha`.
2. Fetch the artifact named by `$ARTIFACT_REF` (drawer id → drawer fetch, or a
   file path → read the file).
   - Drawers are immutable (append-only); the `$ARTIFACT_REF` content cannot
     change, so no hash recompute is needed — the ref is the integrity anchor.
     (For `final` the ref is the user-approved drawer; for `final_review` the
     orchestrator dispatched it directly with no gate.)

## Actions
- If `$TOPIC == final_review`: parse the artifact JSON as
  `{"title":"...","body":"..."}`; derive the base branch from the collab
  `base_sha` (prefer `origin/main`, then `origin/master`, then `origin/trunk`
  when they contain that commit); run `gh pr create --base <base_branch>
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
