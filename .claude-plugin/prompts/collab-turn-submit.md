---
turn: submit
tier: mechanical
model: sonnet
topics: [canonical, final, final_review, failure_report]
preconditions: a prior compose worker wrote $ARTIFACT_REF and the user approved
---

# Collab worker — submit-by-ref (post-gate sender)

> ANTI-PUPPETEERING: You received only this template, `$SESSION_ID`, `$TOPIC`,
> and `$ARTIFACT_REF`. Read the approved artifact by ref and send it. Do NOT
> re-author or editorialize. Your final message MUST be the ≤3-line verdict only.

## State discovery
1. `collab_status(session_id=$SESSION_ID)` to confirm phase/owner and read
   `repo_path`, `branch`, and `base_sha`.
2. Fetch the artifact named by `$ARTIFACT_REF` (drawer id → drawer fetch, or a
   file path → read the file).

## Actions
- If `$TOPIC == final_review`: parse the artifact JSON as
  `{"title":"...","body":"..."}`; derive the base branch from the collab
  `base_sha` (prefer `origin/main`, then `origin/master`, then `origin/trunk`
  when they contain that commit); run `gh pr create --base <base_branch>
  --head $BRANCH --title <title> --body <body>`; capture `pr_url`;
  `collab_send(sender="claude", topic="final_review",
  content=<JSON {"head_sha":"<HEAD>","pr_url":"<url>"}>)`. On `gh` failure or
  base-branch resolution failure, send `failure_report`
  `pr_create_failed:...`.
- Otherwise (`canonical`/`final`): `collab_send(sender="claude", topic="$TOPIC",
  content=<artifact body, JSON-wrapped {"plan":...} only for `final`>)`.

## Verdict
Return EXACTLY these ≤3 lines, nothing else:
```
result: $TOPIC sent
ref: <pr_url | none>
blocker: <one line | none>
```
