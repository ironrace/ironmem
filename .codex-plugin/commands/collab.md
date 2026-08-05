---
description: Join or start an IronMEM bounded Claude/Codex collab session from Codex.
argument-hint: "start [--implementer=claude|codex] <task> | join [--implementer=claude|codex] <session_id>"
---

# /collab

<!-- DERIVED FROM docs/COLLAB.md. This interactive shim selects one installed
phase prompt; Claude's background dispatcher performs the same selection. -->

The user invoked `/collab` with:

```text
$ARGUMENTS
```

You are Codex. This is one-shot: one invocation handles one Codex-owned action
and exits. Use the IronMEM collab tools; if `mcp__ironmem__collab_*` is
unavailable, use tool discovery for `ironmem collab` first.

One collab session may implement only one issue with 1–10 execution tasks. If
planning establishes that the work needs 11 or more, stop before implementation
and split it into independently executable child issues. Route every child
through `/evaluate-issue`; start a separate collab session only for a child
that receives a `COLLAB` verdict.

For `start`, select `collab-plan-draft.md`. For `join`, parse exactly one
session id plus an optional `--implementer=claude|codex` flag. Reject any other
flag or extra value. When that flag is present, call
`collab_set_implementer` with `agent="codex"` before waiting or selecting a
prompt; use its returned session record. That rebinds an active batch before
the phase is routed. Without the flag, call `collab_status`.

Then call `collab_wait_my_turn(session_id, "codex", 60)` once to bridge the
handoff race, refresh `collab_status`, and select the prompt from session state:

| Phase | Prompt |
|---|---|
| `PlanParallelDrafts` | `collab-plan-draft.md` |
| `PlanSynthesisPending` with normal Codex-pilot ownership | `collab-plan-synthesis.md` |
| `PlanCodexReviewPending` | `collab-plan-review.md` |
| `PlanClaudeFinalizePending` with normal Codex-pilot ownership | `collab-plan-finalize.md` |
| `CodeImplementPending` with `implementer == "codex"` | `collab-batch-impl.md` |
| `CodeReviewFixGlobalPending` | `collab-global-review.md` |
| `CodeReviewLocalPending` with normal Codex-pilot ownership | `collab-review-local.md` |
| `CodeReviewFinalPending` with normal Codex-pilot ownership | `collab-final-review.md` |
| `CodeReviewLocalPending` or `CodeReviewFinalPending` with Codex recovery ownership | `collab-recovery.md` |

For the two recovery phases, select `collab-recovery.md` only when
`pending_failure` is non-null and Codex is the recorded recovery owner; otherwise
use the normal-pilot row above. “Normal Codex-pilot ownership” means
`pilot == "codex"`, `current_owner == "codex"`, and no `pending_failure`.
For a normal Claude-owned or terminal phase, report the concise status and exit.
Locate the selected prompt at the first existing root:

1. `$CODEX_HOME/prompts/<selected prompt>`
2. `~/.codex/prompts/<selected prompt>`
3. `.codex-plugin/prompts/<selected prompt>` in the repository
4. `$CODEX_PLUGIN_ROOT/prompts/<selected prompt>`

Read the selected prompt completely and follow it, substituting the original
arguments where it contains `$ARGUMENTS`. Claude's background dispatch uses
the same resolved phase prompt verbatim, never added session summaries.

The dispatcher runs Codex with `-s danger-full-access`: it is required for
linked-worktree git metadata and daemon tests, but it removes containment for
untrusted diff/review content and network egress. Do not run collab against
untrusted-party content. If no selected prompt is installed, ask the user to
run `scripts/install-ironmem.sh` from the IronMEM repository.
