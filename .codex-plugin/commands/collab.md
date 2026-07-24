---
description: Join or start an IronMEM bounded Claude/Codex collab session from Codex.
argument-hint: "start <task> | join [--implementer=claude|codex] <session_id>"
---

# /collab

<!-- DERIVED FROM docs/COLLAB.md. This interactive shim selects one installed
phase prompt; Claude's background dispatcher performs the same selection. -->

The user invoked `/collab` with:

```text
$ARGUMENTS
```

You are Codex. This is one-shot: one invocation handles one Codex-owned action
and exits. Use
the IronMEM collab tools; if `mcp__ironmem__collab_*` is unavailable, use tool
discovery for `ironmem collab` first.

For `start`, select `collab-plan-draft.md`. For `join`, call
`collab_status` first, then select the prompt from session state:

| Phase | Prompt |
|---|---|
| `PlanParallelDrafts` | `collab-plan-draft.md` |
| `PlanCodexReviewPending` | `collab-plan-review.md` |
| `CodeImplementPending` with `implementer == "codex"` | `collab-batch-impl.md` |
| `CodeReviewFixGlobalPending` | `collab-global-review.md` |

For Claude-owned or terminal phases, report the concise status and exit.
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
