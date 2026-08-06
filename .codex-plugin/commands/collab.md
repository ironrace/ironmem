---
description: Join or start an IronMEM bounded Claude/Codex collab session from Codex.
argument-hint: "start [--implementer=claude|codex] [--] <task> | join [--pilot=claude|codex] [--implementer=claude|codex] [--] <session_id>  (`--` ends the flags: everything after it is literal text)"
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

For `start`, select `collab-plan-draft.md`. **`--` ends the flags.** The first
bare `--` token is the end-of-options terminator: every token after it is
literal positional text, never parsed as a flag and never stripped. The `--`
itself is consumed — it is not part of the captured positional. Flags are
recognized only before the first `--`; within that region they may appear
**anywhere in the token stream before the first `--`**, before or after the
task text, and each recognized flag is stripped out of the stream before
`<task>` is captured. `start` takes no `--pilot` flag on this side: reject any
`--pilot` token — `--pilot=claude`, `--pilot=codex`, an unrecognized value,
the bare flag with no `=`, or any other form — as a usage error naming the
offending token and stating that `start` takes no `--pilot` flag on the Codex
side. Never strip it into the task text and never call `collab_start` with a
pilot inferred from it. That rejection binds only tokens before the first
`--`: **a flag-shaped token after the first `--` is not malformed input — it
is literal positional text, and it must never raise a usage error.** **When
the task text legitimately contains a flag-shaped token, put `--` before the
task**: `/collab start -- document how --pilot=codex behaves` records that
whole sentence as the task rather than erroring on it. `<task>` ← the
remaining text after stripping `start`, any recognized flag, and the `--`
terminator if one was given, with every token after that `--` kept verbatim.

For `join`, parse exactly one session id plus an optional
`--pilot=claude|codex` flag and an optional `--implementer=claude|codex` flag.
**`--` ends the flags.** The first bare `--` token is the end-of-options
terminator: every token after it is literal positional text, never parsed as a
flag and never stripped. The `--` itself is consumed — it is not part of the
captured positional. Flags are recognized only before the first `--`; within
that region they may appear **anywhere in the token stream before the first
`--`**, in either order, before or after the id; strip both flag tokens before
capturing the positional id. `<session_id>` ← the single remaining token after
stripping `join`, both flags, and the `--` terminator if one was given, kept
verbatim. Reject any other flag, an extra positional value, an unrecognized
value (`--pilot=gpt`), an empty value (`--pilot=`), the bare flag with no `=`,
or the same flag twice — naming both the offending token and the accepted set
`{claude, codex}`. Never silently fall back to a default on a malformed flag.
These rules bind only tokens before the first `--`: **a flag-shaped token
after the first `--` is not malformed input — it is literal positional text,
and it must never raise a usage error.** **When the session id legitimately
contains a flag-shaped token, put `--` before the id**:
`/collab join -- <session_id>` takes the id verbatim — the consumed `--` is
not the "extra positional value" this parse rejects — and, with neither flag
given, leaves `pilot` and `implementer` both untouched. An absent flag means
"leave that role alone"; only issue a mutation for a flag actually given.

**Call `collab_status` first, before any mutation**, and read `task`, `phase`,
`current_owner`, `pilot`, and `implementer`; every branch below is decided from
that record. Passing `--pilot` is never by itself authorization to change the
pilot — the flag states an intent, and `status.pilot` decides whether that
intent is even attemptable. If `--pilot` was given, branch on `status.pilot` in
**exactly this order**:

1. **Requested pilot matches `status.pilot`** → no-op. **Do not call
   `collab_set_pilot`.** Report the unchanged pilot and continue with the
   status already read. Re-joining with the same `--pilot` is idempotent and
   must not touch the session — including mid-drafting, where an unnecessary
   call would be rejected outright.
2. **Differs and `status.pilot == "codex"`** → authorized: Codex currently
   holds the role and may hand it away. Call `collab_set_pilot` with
   `session_id`, `agent="codex"`, and `pilot=<flag value>` — **before** any
   `collab_set_implementer` call — and use the returned session record as the
   current status from then on (the same update also moves `current_owner` to
   the new pilot, so a stale pre-call status would misroute the very next
   turn).
3. **Differs and `status.pilot != "codex"`** → **fail before attempting the
   mutation.** `collab_set_pilot` is caller-restricted: it checks authorization
   *before* any state check, and only the session's **current** pilot may
   reassign the role. Codex, having already handed the role away, is the
   copilot here and can never reclaim it from this side. Report the current
   pilot and state that reclaiming `pilot=codex` requires a **Claude-side**
   `join --pilot=codex`. **Never call `collab_set_pilot` in this branch, and
   never retry.**

**Authorization is necessary, not sufficient.** Even in the authorized branch
the server applies a second, independent rule: the pilot may only be reassigned
in `PlanParallelDrafts` **and only before either draft lands** (both draft
hashes still unset). A re-join into a session that is already drafting — or
past drafting altogether — is rejected even for a legitimately authorized
caller. When that happens, **surface the server's rejection message verbatim,
preserve the existing pilot, and continue with the session exactly as it
stands — never retry, never overwrite.** Do not pre-empt the server's decision
either: make the one call and report what it says.

When the implementer flag is present, call `collab_set_implementer` with
`agent="codex"` — **after** any pilot change above — before waiting or
selecting a prompt; use its returned session record. That rebinds an active
batch before the phase is routed.

Then call `collab_wait_my_turn(session_id, "codex", 60)` once to bridge the
handoff race, refresh `collab_status`, and select the prompt from session state:

| Phase | Prompt |
|---|---|
| `PlanParallelDrafts` | `collab-plan-draft.md` |
| `PlanSynthesisPending` with normal Codex-pilot ownership | `collab-plan-synthesis.md` |
| `PlanCodexReviewPending` with copilot (Codex-owned) ownership | `collab-plan-review.md` |
| `PlanClaudeFinalizePending` with normal Codex-pilot ownership | `collab-plan-finalize.md` |
| `CodeImplementPending` with `implementer == "codex"` | `collab-batch-impl.md` |
| `CodeReviewFixGlobalPending` with copilot (Codex-owned) ownership | `collab-global-review.md` |
| `CodeReviewLocalPending` with normal Codex-pilot ownership | `collab-review-local.md` |
| `CodeReviewFinalPending` with normal Codex-pilot ownership | `collab-final-review.md` |
| `CodeReviewLocalPending` or `CodeReviewFinalPending` with Codex recovery ownership | `collab-recovery.md` |

For the two recovery phases, select `collab-recovery.md` only when
`pending_failure` is non-null and Codex is the recorded recovery owner; otherwise
use the normal-pilot row above. “Normal Codex-pilot ownership” means
`pilot == "codex"`, `current_owner == "codex"`, and no `pending_failure`.
“Copilot (Codex-owned) ownership” means `current_owner == "codex"` on a
copilot-gated phase — true under the default `pilot == "claude"`; under
`pilot == "codex"` Claude owns those two phases and there is no Codex turn.
For **any phase with no matching row above** — including a normal Claude-owned
or terminal phase — report the concise status and exit. Select nothing.

`PlanLocked` is the one phase that needs saying out loud, because under
`pilot == "codex"` it is Codex-owned and not obviously terminal, and an
installed `collab-task-list.md` sits on disk with a matching name. It routes to
**nothing** on this side under either pilot: report the status, state that the
`task_list` bridge is owned by Claude's dispatcher, and exit. Never select
`collab-task-list.md`. The dispatcher-owned human planning approval gate fires
at `PlanLocked` before any `task_list` send, and this one-shot `codex exec`
cannot prompt a human — sending from here would start autonomous coding on a
plan no human approved.
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
