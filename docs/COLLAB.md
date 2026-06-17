# IronRace Collab (v1 Planning + v3 Coding)

`ironmem` includes a bounded collaboration protocol that lets Claude Code
and Codex coordinate a single plan and then implement it through the shared
MCP server.

- **v1 (planning)**: bounded parallel drafts → canonical synthesis → Codex
  review → Claude finalize → `PlanLocked`. Two review rounds.
- **v3 (coding)**: post-`PlanLocked` task list → **batch implementation
  phase** (Claude publishes the `writing-plans` task document, then the
  session's `implementer` runs per-task subagents via
  `subagent-driven-development` and signals completion with
  `implementation_done`) → global 3-phase linear flow (Codex review+fix →
  Claude local audit → Claude final with PR URL) → `CodingComplete` /
  `CodingFailed`. Per-task implementation is single-agent on the selected
  implementer's side; Codex always owns the first branch-scope review pass.

This document covers:

- the full state machine and invariants (v1 + v3)
- the `collab_*` MCP tools
- topic payload formats for every protocol message
- harness-side responsibilities (git, cargo, gh, pr-review-toolkit)
- Claude's dispatcher loop and one-shot Codex dispatches
- Claude's Plan Mode integration for the first canonical synthesis and the v1 final plan (the two surviving user gates; the v3 bridge and final_review run autonomously after plan-lock)
- copy-pasteable prompts (single-terminal default; Codex-terminal fallback)
- a worked example

The two slash-command prompts that agents actually run are derived from
this spec — keep them in sync when protocol changes land:

- `.claude-plugin/commands/collab.md` — Claude's `/collab` prompt.
- `.codex-plugin/prompts/collab.md` — Codex's `/collab` prompt.

## What It Is

IronRace Collab v1 is a **bounded planning protocol**, not an open-ended
multi-agent framework. Exactly one plan is produced per session, with:

1. two independent first drafts (Claude + Codex, blind to each other)
2. one canonical synthesis by Claude
3. up to two review rounds by Codex
4. one final plan published by Claude (Claude has the last word)
5. terminal state `PlanLocked`

There is no `PlanEscalated` state. After two `request_changes` rounds Claude
is forced to finalize regardless of Codex's objections.

### Review cap (server-enforced)

`MAX_REVIEW_ROUNDS = 2` is the hard cap on Codex plan reviews, enforced
server-side at `crates/ironmem/src/collab/state_machine/mod.rs:28`
(force-finalize branch at `mod.rs:107`). Two review rounds is a maximum,
not an iteration target.

- After Codex's **2nd** `review` message, the server transitions to
  `PlanClaudeFinalizePending` **regardless of verdict** — `approve`,
  `approve_with_minor_edits`, and `request_changes` all map to the same
  next phase.
- Claude has the last word: any unresolved review notes are absorbed (or
  explicitly declined with a rationale) in the `final` plan.
- `review_round` is the audit trail. It is set to 1 after the first
  review and to 2 after the second; post-finalize the test suite asserts
  `review_round == MAX_REVIEW_ROUNDS` at `state_machine/tests.rs:205`.
- The protocol is bounded by construction: at most two Codex reviews,
  then forced Claude finalize. Docs/prompts that frame v1 planning as
  open-ended iteration to convergence are wrong.

## Runtime Model

```text
Claude orchestrator (thin dispatcher loop in one terminal)
  ├─ Agent-tool worker layer — one fresh-context worker per Claude-owned turn
  │    (.claude-plugin/prompts/collab-turn-*.md; orchestrator ingests only the
  │     worker's ≤3-line verdict — full artifacts never transit the orchestrator)
  │     └─ collab_* MCP tools (workers call these directly)
  │         └─ ironmem serve (stdio)
  │             └─ SQLite (sessions, messages, capabilities, wal_log)
  └─ Codex turns dispatched inline via background `codex exec` (one-shot)
       └─ collab_* MCP tools
           └─ ironmem serve (stdio) → SQLite
```

Protocol enforcement lives in the server. The Claude orchestrator is a thin
long-running dispatcher that polls the state machine but does no protocol work
inline: for every Claude-owned turn it spawns ONE fresh-context worker via the
`Agent` tool (the per-turn `collab-turn-*.md` template), and that worker calls
the MCP tools and reads/writes artifacts itself. Codex turns are one-shot
clients dispatched inline that read state, send exactly one protocol message,
and exit. See § "Worker-per-turn dispatch (Claude side)" for the full model.

## Session State

Stored in `collab_sessions`:

| Field | Meaning |
|---|---|
| `id` | Session identifier (returned from `collab_start`) |
| `repo_path`, `branch` | Where this plan applies |
| `task` | Human description of the planning goal. Set at `start`, readable via `status`. |
| `implementer` | Which agent runs the v3 batch implementation phase (`claude` or `codex`). Set at `start` and rebindable with `collab_set_implementer` while planning or `CodeImplementPending` is active. Default `claude`. |
| `phase` | Current protocol phase (see below) |
| `current_owner` | Agent whose turn it is (`claude` or `codex`) |
| `claude_draft_hash`, `codex_draft_hash` | SHA-256 of each first draft |
| `canonical_plan_hash` | SHA-256 of Claude's synthesis |
| `canonical_plan` / `canonical_plan_ref` | The latest `canonical` plan (present when `canonical_plan_hash` is set). By default returned as a compact `canonical_plan_ref` `{drawer_id, hash, first_200_chars}`; with `verbose:true` the full `canonical_plan` string (raw synthesis text — `canonical` has no JSON envelope) is inlined. Lets a fresh agent rejoining mid-planning pull back its own earlier synthesis without a counterpart `recv`. See "Plan-by-reference contract". |
| `canonical_plan_drawer_id` / `final_plan_drawer_id` | Deterministic 32-char id of the `collab-plans` drawer storing the canonical/final plan body once accepted (migration 009). NULL on pre-009 sessions → legacy inline path. |
| `final_plan_hash` | SHA-256 of the locked plan |
| `final_plan` / `final_plan_ref` | The locked `final` plan (present when `final_plan_hash` is set). By default returned as a compact `final_plan_ref` `{drawer_id, hash, first_200_chars}`; with `verbose:true` the full `final_plan` string is inlined as normalized, already-parsed plan text. No caller unwraps `{"plan":...}`, including legacy NULL-drawer sessions. Primary input to the v3 `task_list` bridge after `PlanLocked`. See "Plan-by-reference contract". |
| `codex_review_verdict` | Last Codex verdict |
| `review_round` | Number of completed Codex reviews (0, 1, or 2) |
| `ended_at` | Non-null once `collab_end` has been called |

All state changes are recorded in `wal_log`.

## Phase Model

### `PlanParallelDrafts`

Both agents submit exactly one `draft`. Order is not enforced.

Claude's draft is sent **autonomously — no Plan Mode, no user approval —**
so Codex's draft can begin grinding without waiting on a user gate. The
blind-draft invariant is enforced server-side independent of this gating
change.

**Blind-draft invariant:** `collab_recv` suppresses a counterpart's
`draft` until the calling agent has submitted its own. This is enforced
server-side, not by convention.

Exit: once both draft hashes are present → `PlanSynthesisPending`, owner
`claude`.

### `PlanSynthesisPending`

Owner: `claude`. Claude sends one `canonical` message containing the merged
plan.

This phase is also re-entered on `request_changes`, so Claude uses it both
for the first synthesis and for revisions. **Gating split by `review_round`
(prompt-enforced, not server-enforced):**

- `review_round == 0` (first synthesis) — Claude enters harness Plan Mode
  and gets user approval before sending `canonical`. This is the user's
  primary v1 steering gate (the first artifact that combines both drafts).
- `review_round >= 1` (revision rounds, re-entered on `request_changes`) —
  Claude sends autonomously. The user's next gate is `final` at
  `PlanClaudeFinalizePending`.

The server accepts a `canonical` send in either branch; the user-approval
requirement on the first synthesis is honored by the prompt layer
(`.claude-plugin/commands/collab.md`'s v1 phase table). Compare with
`MAX_REVIEW_ROUNDS = 2`, which IS server-enforced via the force-finalize
branch above. Treat the prompt-enforced split as advisory at the protocol
layer but load-bearing in the dispatcher.

Exit → `PlanCodexReviewPending`, owner `codex`.

### `PlanCodexReviewPending`

Owner: `codex`. Codex sends one `review` with a verdict:

- `approve`
- `approve_with_minor_edits`
- `request_changes`

Exit:

- `approve` or `approve_with_minor_edits` → `PlanClaudeFinalizePending`, owner `claude`.
- `request_changes` and `review_round < 2` → back to `PlanSynthesisPending`, owner `claude`.
- `request_changes` and `review_round >= 2` → `PlanClaudeFinalizePending`, owner `claude` (forced finalize; Claude has the last word).

### `PlanClaudeFinalizePending`

Owner: `claude`. Claude sends one `final` message.

Exit → `PlanLocked` (always). Planning is done.

### `PlanLocked`

Plan is frozen; `final_plan_hash` is set. This is terminal for `wait_my_turn`
**only while `task_list` has not yet been submitted**. Two transitions out:

- `collab_end` — abandon before coding starts (last point this is valid).
- `collab_send` with `topic=task_list` from `claude` — enter the v3 coding
  loop. The state machine verifies `plan_hash == final_plan_hash` and the
  task list is non-empty; the session stays active and the terminal set for
  `wait_my_turn` flips to `{CodingComplete, CodingFailed}`.

## v3 Coding Phase Model

v3 reuses the same session (no new `id`). It extends `collab_sessions` with
a `base_sha` / `last_head_sha` pair for branch-drift detection, `pr_url`
for the PR handoff, and `coding_failure` for unrecoverable errors. Each
phase names the exact event that advances it.

v3 is deliberately linear: every turn deterministically advances to the
next phase. There are no debate rounds, no verdicts, no round counters
at the coding stage. This structurally prevents the orchestrator from
steering the reviewer's conclusion — Codex's only coding turn is at the
global review stage and is expressed as commits, not prose.

### Batch implementation

After `task_list` lands, the session sits in a single phase for the
entire implementation run. Which agent owns that phase depends on the
session's current `implementer` field:

- **`implementer == "claude"`** (default): Claude orchestrates per-task
  work through `writing-plans` (markdown plan) and then
  `subagent-driven-development` (fresh subagent per task,
  TDD, per-task commits). Claude emits `implementation_done`.
- **`implementer == "codex"`** (opt-in via
  `/collab start --implementer=codex` or
  `/collab join --implementer=codex <session_id>`): Claude still produces the
  writing-plans markdown and publishes `task_list` (writing-plans
  approval is still the user gate). Then Claude dispatches Codex via
  background `codex exec` (per Implementation Notes § Background
  `codex exec` dispatch; `mcp__codex__codex` is the fallback when
  `codex` is not on PATH). Codex runs its own
  `subagent-driven-development` (controller-owned loop, runs to
  completion) and emits `implementation_done` itself before the
  bg-exec polling loop detects phase advance.

In both modes the server stores the `task_list` manifest as an audit
artifact but does not iterate it. Per-task progress is observable through
the git log on the branch and through durable ironmem checkpoints written
by the implementer after each task boundary. After `implementation_done`,
the phase advances to `CodeReviewFixGlobalPending` with **Codex** as
owner regardless of who implemented — Codex runs `/pr-review-toolkit:review-pr`
on the raw post-implementation diff first (no Claude pre-clean) and
applies any confirmed fixes directly. Claude's `/ultrareview-local` then
audits Codex's commits at `CodeReviewLocalPending`.

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeImplementPending` | `claude` or `codex` (per session `implementer`) | `ImplementationDone{head_sha}` from the implementer agent — fired once after the full subagent batch completes (gates green, all commits pushed) | `CodeReviewFixGlobalPending` (Codex-owned) |

The `implementation_done` payload carries **only** `head_sha`. There is
no `notes`, `summary`, `subagent_report`, or any other field — the
non-implementer agent reads the diff and the writing-plans markdown in
the repo (via `plan_file_path`) at the global review stage and forms
its own judgment.

### Implementation checkpoints

During `CodeImplementPending`, the implementer must write durable
checkpoints to ironmem so a fresh Claude or Codex process can resume if
the current session stops mid-batch. These checkpoints do **not** advance
the collab state machine and must not be sent through `collab_send`.

Checkpoint storage:

- Tool: `mcp__ironmem__add_drawer`
- `wing`: `ironrace-memory`
- `room`: `collab-checkpoints`
- One checkpoint before starting each task (`status: started`)
- One checkpoint after each task is implemented, reviewed, committed, and
  pushed (`status: completed`)
- One checkpoint on unrecoverable task failure (`status: blocked`)
- One final checkpoint before `implementation_done`
  (`status: batch_complete`)

Checkpoint content should be compact, line-oriented, and include enough
state for another agent to resume without transcript context:

```text
collab_checkpoint
session_id: <uuid>
phase: CodeImplementPending
implementer: <claude|codex>
repo_path: <absolute repo path>
branch: <branch>
plan_file_path: <repo-relative plan path>
task_id: <N|none>
task_title: <title|none>
status: <started|completed|blocked|batch_complete>
head_sha: <current HEAD>
commit_sha: <task commit sha|none>
completed_task_ids: <comma-separated ids>
next_task_id: <N|none>
gates: <not_run|passed|failed: short reason>
summary: <one concise sentence>
resume_hint: /collab join [--implementer=<claude|codex>] <session_id>
```

On any fresh `/collab join` that lands in `CodeImplementPending`, the
owning implementer must first search `wing=ironrace-memory`,
`room=collab-checkpoints` for the session id and use the newest
checkpoint plus the git log to choose the first unfinished task. The new
implementer must then read the plan and scan the current code/diff to
verify which acceptance criteria are already complete before editing. If
the newest checkpoint is `batch_complete`, rerun the required gates and
send `implementation_done`; otherwise resume at `next_task_id` (or the
`started` task if the last checkpoint stopped mid-task).

**Both modes apply the same `finishing-a-development-branch` carve-out**:
the implementer agent stops `subagent-driven-development` at the last
task's approval+commit and does *not* let the skill auto-invoke
`finishing-a-development-branch`. PR creation belongs to
the collab `final_review` turn, not to the subagent skill.

### Global review, 3-phase linear (Codex first; Claude audits after)

After `implementation_done`, the session enters a 3-turn linear review at
branch scope. Codex runs `/pr-review-toolkit:review-pr` on the raw
post-implementation diff first; Claude then audits Codex's commits via
`/ultrareview-local`; Claude opens the PR on the final turn.

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeReviewFixGlobalPending` | `codex` | `CodeReviewFixGlobal{head_sha}` — Codex ran `/pr-review-toolkit:review-pr` on the full diff AS-IS (no Claude pre-clean) and (if needed) pushed fixes directly | `CodeReviewLocalPending` |
| `CodeReviewLocalPending` | `claude` | `ReviewLocal{head_sha}` — Claude ran `/ultrareview-local` as an audit of Codex's commits + code-quality issues both agents missed; pushed any fixes | `CodeReviewFinalPending` |
| `CodeReviewFinalPending` | `claude` | `FinalReview{head_sha, pr_url}` — Claude opens the PR and sends the URL in the same event | `CodingComplete` (terminal) |

### Shortcut: post-subagent coding review

When an orchestrator already completed the branch's implementation outside
Collab, it can skip v1 planning and the v3 batch implementation phase by
calling `collab_start_code_review`. The session starts directly at
`CodeReviewFixGlobalPending` with `current_owner = codex`.

Because shortcut sessions have no collab `task_list`, Codex must recover
the implementation context before reviewing: search ironmem checkpoints
for the same `repo_path`/`branch`, read any referenced writing-plans
markdown, and scan the current code/diff to determine which acceptance
criteria are already complete. If no checkpoint exists, fall back to the
branch diff plus nearby writing-plans docs in the repo.

The no-op handshake turn is collapsed: `head_sha` is supplied at session
creation time. From there, the surviving flow follows the new ordering:
Codex `review_fix_global` (`/pr-review-toolkit:review-pr` plus confirmed
fixes on the raw diff) → Claude `review_local` (audit Codex's commits)
→ Claude `final_review` (PR creation).

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeReviewFixGlobalPending` | `codex` | `CodeReviewFixGlobal{head_sha}` | `CodeReviewLocalPending` |
| `CodeReviewLocalPending` | `claude` | `ReviewLocal{head_sha}` | `CodeReviewFinalPending` |
| `CodeReviewFinalPending` | `claude` | `FinalReview{head_sha, pr_url}` | `CodingComplete` |

Invariants that still apply:

- `collab_end` is rejected during all review phases, same as any other
  coding-active phase.
- `failure_report` is the only escape hatch and transitions to
  `CodingFailed`.
- Drift detection is special-cased for shortcut-started sessions:
  the server validates `CodeReviewFixGlobal{head_sha}` **and**
  `ReviewLocal{head_sha}` with a git ancestry check when `task_list` is
  still unset. Both Codex's `review_fix_global` push and Claude's
  `review_local` audit-push must descend from the prior `last_head_sha`.
  Full-flow v3 sessions keep their existing non-shell-out behavior.
- **Process attribution:** one active collab session per MCP server process
  (any repo). On the error `"another active collab session is already bound to
  this MCP process for metrics attribution: <id>"`, do not retry blindly —
  `collab_end` the named session if it is finished, or run the new session from
  a separate server process.

### Deployment

This change is forward-only; the collab state machine has no
protocol-version field. Operational rollout:

1. Pause / avoid starting new coding-phase collab sessions before rollout.
2. Drain existing coding-active sessions to `CodingComplete` /
   `CodingFailed`, or abort them.
3. Deploy; new sessions start under new ordering.

Sessions stored mid-coding-phase that survive deployment will follow the
new transition semantics from their stored phase forward — there is no
migration logic.

### Failure + terminal

| Phase | Owner | Event | Next |
|---|---|---|---|
| *any coding-active phase* | either | `FailureReport{coding_failure}` | `CodingFailed` (terminal) |

`collab_end` is **rejected** in every coding-active phase
(`CodeImplementPending`, `CodeReviewFixGlobalPending`,
`CodeReviewLocalPending`, `CodeReviewFinalPending`). Only
`CodingComplete` or `CodingFailed` end the session post-`task_list`.

## Blind-Draft Invariant

During `PlanParallelDrafts`, neither agent can see the other's draft until
it has submitted its own. This prevents drift toward the first draft that
lands.

Enforcement: `collab_recv` filters out `draft` topic messages from
the counterpart whenever the caller has not yet submitted its own draft.

## MCP Tools

### `collab_start`

Creates a new session.

```json
{
  "repo_path": "/path/to/repo",
  "branch": "feat/landing-page",
  "initiator": "claude",
  "task": "design the marketing landing page",
  "implementer": "claude"
}
```

Returns `{ session_id, task, implementer }`. The `task` is stored on the
session so the counterpart agent can read it via `collab_status` without
a manual paste. `implementer` is optional, defaults to `"claude"`, and
must be one of `{"claude","codex"}` — it routes the v3
`CodeImplementPending` phase to the named agent. The DB CHECK constraint
on the `implementer` column enforces the same set, so direct writes
cannot bypass validation.

**Duplicate-session guard.** `collab_start` (and `collab_start_code_review`)
reject the call when an **active** session already exists for the same
`repo_path` + `branch`. "Active" means `ended_at IS NULL` — including a
session sitting at a terminal phase (`CodingComplete` / `CodingFailed`) that
was never explicitly ended. The error names the existing `session_id` and its
phase. This prevents an accidental second session — most often a fired
`ScheduleWakeup` replaying the `/collab start` entry command after the first
session already finished. To proceed deliberately, either resume the existing
session (`/collab join <id>`) or `collab_end` it first (valid only from
`PlanLocked` pre-`task_list`, `CodingComplete`, or `CodingFailed`).

**Process attribution guard.** `collab_start`, `collab_start_code_review`,
`collab_send`, `collab_recv`, and `collab_wait_my_turn` also reject the call
when this MCP server process is already bound to a *different* still-live
session for metrics attribution (regardless of repo or branch). The error
message is: `"another active collab session is already bound to this MCP
process for metrics attribution: <id>. End it or use a separate server process
before switching to <requested_id>."` Remedy: call `collab_end` on the named
session if it is finished, or run the new session from a separate server
process. Stale or ended sessions self-clear automatically — no manual cleanup
is needed for those. On the error `"could not verify active collab session"`,
check the server logs for the underlying DB error detail; retry after the
underlying issue clears.

### `collab_set_implementer`

Rebinds the batch implementation owner for an existing session.

```json
{
  "session_id": "...",
  "agent": "claude",
  "implementer": "codex"
}
```

Valid during planning and during `CodeImplementPending`. During
`CodeImplementPending`, this also moves `current_owner` to the selected
implementer so `/collab join --implementer=<agent> <session_id>` can hand
the active batch to another agent. The new implementer resumes from the
latest ironmem checkpoint, then scans the plan and current code before
continuing. Calls after `implementation_done` are rejected.

### `collab_start_code_review`

Shortcut entry. Creates a session positioned at `CodeReviewFixGlobalPending`,
owner `codex`. See the "Shortcut: post-subagent coding review" subsection
above for the constraints and surviving flow.

```json
{
  "repo_path": "/path/to/repo",
  "branch": "feat/landing-page",
  "base_sha": "abc123",
  "head_sha": "def456",
  "initiator": "claude",
  "task": "add landing page"
}
```

Returns `{ session_id, task }`. The `task` is stored on the session and is
readable via `collab_status`.

### `collab_send`

Sends a protocol message and advances the state machine.

```json
{ "session_id": "...", "sender": "claude", "topic": "draft", "content": "..." }
```

v1 planning topics: `draft`, `canonical`, `review`, `final`.

v3 coding topics: `task_list`, `implementation_done`, `review_local`,
`review_fix_global`, `final_review`, `failure_report`.

The phase→topic acceptance matrix is tabulated in
[§ Phase → Topic Acceptance](#phase--topic-acceptance); consult that table
before every `collab_send`.

### `collab_recv`

Returns pending messages. Enforces the blind-draft invariant.

**Parameters:**

| Parameter | Type | Default | Description |
|---|---|---|---|
| `session_id` | string | required | Session to receive from |
| `receiver` | string | required | `"claude"` or `"codex"` |
| `limit` | integer | 10 (max 50) | Maximum messages to return |
| `auto_ack` | boolean | `false` | When `true`, atomically marks all returned messages as acked in the same DB transaction as the read. Eliminates a separate `collab_ack` round-trip per turn. Backwards-compatible — existing callers that omit the field continue to use the two-step recv + ack flow. |

Using `auto_ack=true` is recommended in the dispatch loop for any caller that
always acks all received messages immediately. The explicit `collab_ack` call
is still available when callers need selective acknowledgement.

### `collab_ack`

Marks a message consumed. Session-scoped: a mismatched
`(session_id, message_id)` pair is rejected. The generation guard resolves
the acting agent from the target message's `receiver` field (`collab_ack`
takes no `agent` parameter), so the lease guard applies to the message's
receiver.

### `collab_status`

Returns the full session record including `phase`, `current_owner`, `task`,
`review_round`, `ended_at`, and all hashes. Call this before every protocol
action.

#### Plan-by-reference contract

Accepted plan bodies are returned by reference by default to keep the status
payload compact:

- **Default (`verbose` false or omitted):** the accepted `canonical` and
  `final` plans are returned as compact references —
  `canonical_plan_ref` / `final_plan_ref` = `{drawer_id, hash,
  first_200_chars}`. The full plan strings are omitted.
- **`verbose:true`:** additionally inlines the full `canonical_plan` /
  `final_plan` string. Callers that need the approved plan body (e.g. the v3
  `task_list` bridge, or recovering a prior canonical on a revision round)
  must pass `verbose:true`.
- The `final` body exposed by `collab_status` is the **already-parsed plan
  text** — consumers no longer need to unwrap the legacy
  `{"plan":"<full text>"}` JSON. For post-009 sessions, the drawer stores
  that parsed body; for legacy NULL-drawer sessions, `collab_status`
  normalizes the raw stored message on read. The `hash` verifies the parsed
  body.
- **Backward compatibility:** pre-009 sessions (NULL drawer id) keep the
  legacy inline path — the full plan text is inlined under
  `canonical_plan` / `final_plan` regardless of `verbose`, with `final_plan`
  normalized to parsed plan text. These sessions emit **no**
  `canonical_plan_ref` / `final_plan_ref`, so callers must not assume the
  compact reference object is always present.
- **Recall note:** accepted plan bodies are filed as drawers in the
  dedicated `collab-plans` room with a zero embedding (kept out of vector
  recall), but the generic drawer FTS index still sees their content, so an
  unscoped keyword `search` can surface them. This is an accepted tradeoff
  for issue #90; excluding `collab-plans` from default recall is tracked as a
  follow-up.

### `collab_approve`

Codex-only shortcut for an `approve` review. Requires `content_hash` to
match the stored `canonical_plan_hash`.

### `collab_wait_my_turn` (long-poll)

Blocks server-side until the caller is the owner, the session ends, the
phase becomes terminal (`PlanLocked`), or `timeout_secs` elapses.

```json
{ "session_id": "...", "agent": "claude", "timeout_secs": 30 }
```

Returns `{ is_my_turn, phase, current_owner, session_ended }`. Default
timeout 30s, max 60s. Agents loop on this instead of polling `status` on a
fixed interval.

### `collab_register_caps` / `collab_get_caps`

Advisory: each agent registers available sub-agents/tools so the other can
plan around them.

### `collab_end`

Ends a session. Valid **only** from one of three phases:

- `PlanLocked` pre-`task_list` (the user abandons the plan before coding),
- `CodingComplete` (post-PR),
- `CodingFailed` (after `failure_report`).

**Rejected** during any active planning phase (`PlanParallelDrafts` through
`PlanClaudeFinalizePending`) or coding-active phase (`CodeImplementPending`
through `CodeReviewFinalPending`). This prevents either agent from killing
a session the counterpart is still working in.

Idempotent once allowed: calling from a terminal phase or an
already-ended session is a no-op, and subsequent `send`, `ack`, `approve`,
`register_caps`, and `wait_my_turn` calls all treat the session as ended.

### `session_handoff` (fallback succession)

Issues a cryptographic succession token that lets a fresh process take over
an active session without restarting it. This is the fallback path for an
agent whose context is exhausted mid-session — the successor presents the
token to claim the generation lease and becomes the active process.

**Arguments:** `{ session_id, agent }` plus optional `handoff_token` (for
re-issuance to a third successor). **Returns:** `{ session_id, agent,
generation, handoff_token, handoff_block }` where `generation` is the
**pending (to-be-claimed) generation** = active_generation + 1, not the
caller's current active generation.

**What it does.** The server reads persisted session state and the newest
`collab-checkpoints` drawer for the session and composes a deterministic,
model-free fenced markdown block (` ```ironrace-session-handoff `) — it
NEVER asks a model to summarize. This tool is a WRITE tool and is denied in
read-only / restricted MCP mode.

**Generation lease.** Each `(session_id, agent)` pair tracks an `active
generation` and a `pending_handoff_generation`. `session_handoff` issues (or
byte-identically reuses) a one-time `handoff_token` and sets
`pending_handoff_generation = active_generation + 1` **without** advancing
the active generation. A successor presents the `handoff_token` on its first
actor-bearing mutating/binding collab call (`collab_send`, `collab_recv`,
`collab_ack`, `collab_approve`, `collab_set_implementer`,
`collab_register_caps`, `collab_wait_my_turn`, `collab_end`, or
`session_handoff` itself) to **claim** — the claim advances the active
generation, making the predecessor process **inert**.

**Inertness.** A process whose cached active generation is behind the DB is
rejected from all mutating/binding calls listed above. Pure reads
(`collab_status`, `collab_get_caps`) remain available to a stale
predecessor.

**Token placement.** The `handoff_token` travels at the top level of the
response, NOT inside the fenced block. The successor needs both: the block
(context) and the top-level token (claim credential). Never embed the token
inside the `handoff_block`.

**gen > 0 tokenless rejection.** Once any handoff has been claimed (active
generation > 0), a fresh process with no cached generation and no token is
rejected — it must present a `session_handoff` token. Tokenless first-touch
is permitted only at generation 0 (a session that has never been handed off).

**collab_ack actor resolution.** `collab_ack` has no `agent` parameter; it
resolves the actor from the target message's `receiver` field before
applying the generation guard.

**Self-guard.** `session_handoff` is itself generation-guarded. A stale or
tokenless gen > 0 caller cannot mint a new token after a successor has
claimed — resurrection is closed.

**collab_status additions.** `collab_status` now returns `claude_generation`,
`codex_generation`, `claude_handoff_pending`, and `codex_handoff_pending`
(boolean). The token value itself is never exposed through `collab_status`.

### Context-occupancy handoff

The UserPromptSubmit hook injects a one-line notice when context occupancy
crosses a threshold (default 60% warn / 80% handoff, overridable via
`IRONMEM_CONTEXT_WARN_PCT` / `IRONMEM_CONTEXT_HANDOFF_PCT`).

**Automated successor path (autonomous/collab phases):**

1. On a `>= 80%` (Handoff) notice, the active agent calls
   `session_handoff(session_id, agent)` and captures the **top-level**
   `handoff_token` and `handoff_block` from the response (the token is NOT
   inside the fenced block).
2. The `task_outcomes.handoffs` counter is incremented automatically inside
   `handle_session_handoff` (via `increment_task_handoffs(session_id)` called
   only when a **fresh** token is issued, gated on `!issued.reused`; reusing an
   existing token does NOT increment it). The count reflects handoff
   **intent** at fresh-issue time, not successor claim.
3. Spawn the successor via background Bash, using the same background-Bash
   dispatch pattern documented for Codex (see "Background `codex exec`
   dispatch"); for a `claude -p` successor the command is `claude -p`, not
   `codex exec`:
   ```
   claude -p "join ironmem collab <sid> with token <handoff_token>"
   ```
   with `run_in_background: true`. The successor's first mutating call
   presents the token and claims the lease.
4. The predecessor ends its turn. Once the successor claims the lease (gen+1),
   the predecessor's next mutating call is rejected with "stale collab
   generation" (stale-gen rejection, enforced by
   `ensure_actor_generation_current`). **No process coordination is required —
   the generation lease is the single writer.**

**Cron fallback (where a spawned child cannot outlive its parent):**

When the runtime cannot keep a spawned child alive after the parent exits,
use a one-time local cron entry as a fallback:

```sh
# Add a one-shot entry (runs at the next minute; self-deletes only on success).
# Replace <sid> and <token> with the values from session_handoff.
(crontab -l 2>/dev/null; echo "* * * * * claude -p \"join ironmem collab <sid> with token <token>\" && crontab -l | grep -v 'join ironmem collab <sid>' | crontab -") | crontab -
```

This is a **best-effort fallback only**: local-only, never committed to the
repo. The `&&` means it self-deletes only after the first *successful* join; a
failing join leaves the entry in place, so it re-fires every minute until the
join succeeds or the entry is removed manually.

> **Safety:** `<sid>` and `<token>` must contain only `[A-Za-z0-9_-]`.
> ironmem-issued session IDs are already sanitized to that set, so they are
> shell-safe inside this `crontab` pipeline. Never substitute a raw value from
> an untrusted source — shell metacharacters (`` ` ``, `$(...)`) would execute
> on the host. If in doubt, assign on a separate line and single-quote:
> `SID='...'; TOKEN='...'`.

**Interactive phases (manual flow):**

When the context occupancy notice appears in an interactive session, the user
manually handles the handoff:
1. Note the `session_id` in the notice (the Handoff notice includes
   `join collab <sid>` for easy copy).
2. Call `session_handoff(session_id, agent)` to mint the token (or ask the
   agent to call it).
3. Run `/clear` to reset context.
4. Rejoin with `join collab <sid>` (or `join collab <sid> with token <token>`
   if the token was captured).

**Permission allowlist for unattended successor operation:**

An unattended `claude -p` successor needs at minimum:
- `mcp__ironmem__collab_send` — send phase messages
- `mcp__ironmem__collab_recv` — receive phase messages
- `mcp__ironmem__collab_ack` — acknowledge messages
- `mcp__ironmem__collab_approve` — approve plans/reviews
- `mcp__ironmem__collab_set_implementer` — set implementer
- `mcp__ironmem__collab_register_caps` — register capabilities
- `mcp__ironmem__collab_wait_my_turn` — wait for turn
- `mcp__ironmem__collab_end` — end session
- `mcp__ironmem__session_handoff` — re-handoff if needed
- `mcp__ironmem__collab_status` — read session state
- `Bash(claude -p "join ironmem collab *":*)` — re-spawn a further successor if
  needed. Scope the wildcard to the known join-command form; avoid the broader
  `Bash(claude -p:*)`, which would let the successor spawn arbitrarily-prompted
  sub-agents.
- Git bash operations (`Bash(git commit:*)`, `Bash(git push:*)`, etc.) as
  needed for the implementation tasks the successor will perform.

Operators should configure these in `.claude/settings.json` under
`permissions.allow` before running unattended.

## Payload Formats

### Draft / Canonical / Final

Plain text. Recommended structure:

```text
Goal
- ...

Constraints
- ...

Plan
1. ...
2. ...

Risks
- ...
```

### Review

JSON:

```json
{
  "verdict": "approve_with_minor_edits",
  "notes": ["prefer X over Y", "add rollback step"]
}
```

### Final (JSON envelope)

```json
{ "plan": "final merged plan text" }
```

### v3 coding topic payloads

Every v3 `collab_send` content is JSON. The server parses strictly — missing
or empty required fields reject with a validation error. `head_sha` appears
on every coding message so the server can record branch progress and either
agent can detect drift.

The `implementation_done` payload carries **only** `head_sha`. There is no
`verdict`, `notes`, `comment`, or `subagent_report` field — Codex reads
the diff and the writing-plans markdown in the repo at the global review
stage and forms its own judgment. This is the rule that prevents the
orchestrator from steering the reviewer's conclusion.

| Topic | Sender | Payload | Notes |
|---|---|---|---|
| `task_list` | `claude` | `{"plan_hash","base_sha","head_sha","plan_file_path"?,"execution_mode"?,"tasks":[{"id","title","acceptance":[...]}]}` | `plan_hash` must equal `final_plan_hash`; `tasks` must be non-empty and strictly ordered by `id`; each task requires ≥1 `acceptance` entry. Optional `plan_file_path` (repo-relative; no leading `/`; no `..` segments) points at the writing-plans markdown driving subagent execution. Optional `execution_mode` — see below. |
| `implementation_done` | `claude` or `codex` (per session `implementer`) | `{"head_sha"}` | In `CodeImplementPending` only. Fired once after the subagent batch completes and gates pass. Carries only `head_sha` — no prose, no subagent notes. |
| `review_fix_global` | `codex` | `{"head_sha"}` | In `CodeReviewFixGlobalPending` only. Codex ran `/pr-review-toolkit:review-pr` on the raw post-implementation diff (no Claude pre-clean) and has pushed any branch-level fixes. |
| `review_local` | `claude` | `{"head_sha"}` | In `CodeReviewLocalPending` only. Claude ran `/ultrareview-local` as an audit of Codex's `review_fix_global` commits + caught code-quality issues both agents missed; has pushed any fixes. |
| `final_review` | `claude` | `{"head_sha","pr_url"}` | In `CodeReviewFinalPending` only. Claude has opened the PR; the event carries the URL and advances directly to `CodingComplete`. `pr_url` must start with `https://` and be ≤2048 chars. |
| `failure_report` | either | `{"coding_failure":"<reason>"}` | Valid in any coding-active phase. |

### `task_list` — `execution_mode` field

The optional `execution_mode` string field on the `task_list` payload selects
the implementation strategy for the batch phase. It is validated at send time
and exposed as a top-level `execution_mode` field in `collab_status` so both
agents can read it without re-parsing the canonicalized `task_list` JSON.

| Value | Behaviour |
|---|---|
| *(omitted)* | Default: subagent-driven. The implementer agent invokes `subagent-driven-development` (one subagent per task). |
| `"mechanical_direct"` | Single-task verbatim plan. The implementer applies the plan's bash/code blocks directly without spawning `subagent-driven-development`. |

**Validation rules (server-enforced):**

- Unknown values are rejected immediately with a clear error message listing
  the allowed set. A typo therefore fails at submit time rather than silently
  falling through to the default.
- `"subagent_driven"` is intentionally NOT an allowed value — callers omit
  the field entirely to select the default path. Sending it explicitly is a
  validation error.
- The field is preserved verbatim in the canonicalized `task_list` JSON
  stored on the session, so it survives the round-trip back through
  `collab_status.execution_mode`.

**Eligibility criteria for `"mechanical_direct"` (Claude-side detection).** Set
this mode when ALL four conditions hold:

1. The writing-plans markdown produced exactly one task (`### Task 1` only).
2. The task's `Files:` block lists one or zero files to create or modify.
3. The task's steps include at least one verbatim ` ```bash ` or language code
   block meant to be applied as-is (not pseudocode or illustrative snippets).
4. No step contains language like "decide", "choose between", or other
   design-judgment cues.

When conditions are not met, omit the field — the server treats absence as the
default subagent-driven path.

### Phase → Topic Acceptance

The server dispatches strictly on the current phase. Each topic maps to
exactly one event variant — there is no phase overloading.

| Phase | Accepted topic(s) | Notes |
|---|---|---|
| `PlanParallelDrafts` | `draft` | v1 planning |
| `PlanSynthesisPending` | `canonical` | v1 planning |
| `PlanCodexReviewPending` | `review` | v1 — Codex review of canonical |
| `PlanClaudeFinalizePending` | `final` | v1 — Claude finalizes |
| `PlanLocked` | `task_list` | v1 → v3 hand-off |
| `CodeImplementPending` | `implementation_done`, `failure_report` | v3 — single implementer turn after subagent batch |
| `CodeReviewFixGlobalPending` | `review_fix_global`, `failure_report` | v3 — Codex runs `/pr-review-toolkit:review-pr` on the raw post-implementation diff (first review pass) |
| `CodeReviewLocalPending` | `review_local`, `failure_report` | v3 — Claude audits Codex's commits via `/ultrareview-local` |
| `CodeReviewFinalPending` | `final_review`, `failure_report` | v3 — Claude opens PR |
| `CodingComplete` / `CodingFailed` | *(none — terminal; only `collab_end` accepted)* | |

`failure_report` is accepted from either agent in any coding-active phase
and transitions the session to `CodingFailed`. All other topics are gated
by the owner recorded in the phase table above.

## Harness-Side Responsibilities

The server validates transitions, persists hashes, and routes messages.
Most shell-level action — cargo, gh, pr-review-toolkit — is the **agent harness's**
responsibility. The protocol relies on the harness doing these things
before each coding-active `collab_send`:

- **`base_sha` / `head_sha` tracking.** The harness records `base_sha` at
  `task_list` send time (the commit the branch forked from) and the current
  `head_sha` on every subsequent send. Before acting on an incoming turn,
  the harness reads `last_head_sha` from `collab_status` and runs
  `git cat-file -e <sha>^{commit}` to verify the commit is present; if not,
  it sends `failure_report` with `coding_failure: "branch_drift: ..."`.
- **Pre-send harness fast-path.** Before running `git fetch`, `git checkout`,
  and `git reset --hard` to sync the working tree to `last_head_sha`, the
  harness first checks: is `git rev-parse HEAD` already equal to
  `last_head_sha` AND is the current branch already the session branch? When
  both hold, steps 3 (`git fetch`) and 5 (`git checkout` + `git reset
  --hard`) are skipped entirely. The `git cat-file -e` sanity check (step 4)
  still runs because the commit is already local. This avoids a network
  round-trip and a working-tree reset on the common case where the agent is
  already at the right SHA — for example, entering the batch-impl turn
  immediately after `task_list` is sent.
- **Subagent orchestration** during `CodeImplementPending`. Claude invokes
  `writing-plans` to expand the locked plan into a markdown task document
  and publishes it via `task_list`. The selected `implementer` then runs
  `subagent-driven-development` to dispatch fresh subagents per task. Each
  subagent runs TDD and commits on the branch. Per-subagent failures pause
  for triage; an unrecoverable failure surfaces as `failure_report` with
  `coding_failure: "subagent_failure: ..."`.
- **Implementation checkpoints** during `CodeImplementPending`. The
  implementer writes `ironrace-memory/collab-checkpoints` drawers before
  each task starts, after each task completes, on blocked failures, and
  after final gates pass. On a fresh join at `CodeImplementPending`, the
  implementer searches those checkpoints by `session_id` before doing
  work and resumes from the first unfinished task instead of relying on
  transcript context.
- **Local gates** before every Claude-owned coding turn
  (`implementation_done` in Claude-implementer mode, `review_local`,
  `final_review`): `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test --workspace`. In Codex-implementer mode, Codex runs its own
  gates before sending `implementation_done`. Any failure surfaces as
  `failure_report`; don't hide it.
- **Review + fix tooling** during Codex's `review_fix_global`:
  `/pr-review-toolkit:review-pr` runs as the final Codex review pass
  over the raw post-implementation diff, alongside the writing-plans
  markdown when available. The collab `base_sha` and `last_head_sha`
  define the review target; Codex must not let the toolkit silently
  substitute a different base branch. Codex treats the toolkit output as
  a read-only finding pass, independently verifies findings, then makes
  direct code edits + commit + push for confirmed branch-level issues.
  `/ultrareview-local` is NOT used here — it runs later at Claude's
  `review_local` audit step. Codex's judgment is expressed as commits,
  not prose.
- **Audit tooling** during Claude's `review_local`: `/ultrareview-local`
  (code-reviewer + security-reviewer + architect + doc-reviewer in
  parallel) runs against the post-`review_fix_global` head, auditing
  Codex's commits + catching code-quality issues both agents missed.
  Claude fixes any CRITICAL/HIGH/MEDIUM findings in place + commits + pushes.
- **Shortcut ancestry validation** during shortcut-started
  `review_fix_global` and `review_local`: the server shells out narrowly
  to `git merge-base --is-ancestor` to distinguish a true descendant
  check from operational git failures, and only applies that validation
  when `task_list` is still unset. Both Codex's `review_fix_global` push
  and Claude's `review_local` audit-push must descend from the prior
  `last_head_sha`.
- **PR creation** during `final_review`: Claude resolves a base branch from
  the recorded `base_sha` (preferring `origin/main`, then `origin/master`, then
  `origin/trunk` when they contain that commit), runs `gh pr create --base
  <base_branch> ...`, and sends the URL inline with the `final_review` event.
  There is no separate `pr_opened` turn.
- **Codex must not create or check for PRs.** Codex never calls `gh pr
  create`, `gh pr list`, `git ls-remote refs/pull/*`, or any other
  PR-related GitHub API operation during any of its phases. PR creation
  belongs exclusively to Claude's `final_review` turn. This boundary is
  explicit: removing the PR check from Codex's batch turn also removes
  Codex's dependency on `api.github.com` reachability, which was observed
  as a fragility in practice.
- **Plan Mode** on Claude's side is entered before only two gates: the first
  `canonical` (`review_round == 0`) and `final` (v1). Revision-round canonicals
  run autonomously. The `task_list` send is gated by the orchestrator's
  fidelity check on the worker-produced plan (auto-approve on pass, manual
  reference-only fallback on fail); `writing-plans` must run produce-only and
  must not surface its interactive handoff in this bridge. The `final_review`
  PR creation is autonomous (no gate). Codex never enters Plan Mode.

The server does not read the git tree for the full v3 flow, and it still
trusts the harness's `head_sha` string there. The narrow shortcut-only
ancestry check is the exception; drift detection in that path is now a
hybrid responsibility, with the server performing the git ancestor check
and the harness still responsible for local verification and any
`failure_report` it emits.

## Worker-per-turn dispatch (Claude side)

On the Claude side, `/collab` is a thin orchestrator. For every Claude-owned
protocol turn it does not do the work inline; it: reads a slim `collab_status`
→ spawns ONE fresh-context worker via the `Agent` tool → ingests ONLY the
worker's ≤3-line verdict → loops. The worker prompt is the verbatim
`.claude-plugin/prompts/collab-turn-<turn>.md` template with the `$VAR`
placeholders substituted (`$SESSION_ID`, plus `$REPO_PATH`, `$BRANCH`,
`$TOPIC`, `$ARTIFACT_REF`, `$ARTIFACT_HASH`, `$MODE` where that template uses
them). The worker
calls the ironmem MCP tools directly and reads/writes artifacts via drawers and
files. **Full artifacts — plans, diffs, review reports, PR bodies — never
transit the orchestrator.**

**Anti-puppeteering.** The orchestrator passes ONLY the resolved template. It
never appends an inline recap, a state summary, or "what to conclude." Each
worker discovers state for itself via its own `collab_status` / `collab_recv` /
drawer fetches. This structurally removes the channel the orchestrator could
otherwise use to steer a worker's judgment (mirrors the v3 design that keeps
Claude from steering Codex's review).

**Verdict contract.** The worker's final message is at most three lines
(`result:` / `ref:` / `blocker:`). The orchestrator stores only this verdict;
it does not ingest the body of whatever the worker produced.

### Model tiers + fail-closed

Workers are dispatched at one of three tiers. Fable is OFF, so planning and
review both run on Opus; mechanical turns run on Sonnet/default. The Codex side
is unchanged (xhigh).

| Tier | Turns | Dispatch |
|---|---|---|
| `planning` | `draft`, `canonical` (synthesis), `final` (finalize), `task_list` | `Agent(model=opus)` at max effort |
| `review` | `review_local`, `final_review` | `Agent(model=opus)` |
| `mechanical` | `code-implement` controller, `submit` | `Agent(model=sonnet)` / default |

"Max effort" is the harness thinking-budget mechanism. **Fail-closed rule: if
the harness cannot select the requested tier for a planning or review dispatch,
ABORT the turn and surface to the user — never silently fall back to a lower
tier.**

### Approval gates are reference-only

The two user gates (the first `canonical` and `final`) — plus the PlanLocked
bridge's fidelity-fail fallback — use a two-phase reference-only split so the
orchestrator never has to ingest a full artifact to gate it:

1. **Compose worker** writes the artifact to a drawer and returns
   `{ref, ≤3-line summary}`.
2. **Orchestrator gate** surfaces ONLY `ref + summary` for the user's
   approval — never the full body.
3. **Submit worker** (`collab-turn-submit.md`) reads the approved artifact by
   `$ARTIFACT_REF` and sends it. **Drawer immutability is the integrity
   anchor:** drawers are append-only, so the approved `drawer_id`'s content
   cannot change — the ref itself guarantees the user approved exactly what is
   sent, and no hash recompute is needed (a cross-worker recompute would be both
   redundant and non-reproducible across readback/encoding). It never
   re-authors. If the artifact cannot be fetched, the submit worker does not
   send the protocol topic: for `final_review` (coding-active) it sends a
   `failure_report`; for the v1 planning topics `canonical`/`final` the state
   machine rejects `failure_report`, so it aborts with a `blocker:` verdict
   instead.

The same two-phase compose→submit machinery also drives two **autonomous**
post-plan-lock steps that have **no user gate**: the PlanLocked bridge (the
orchestrator auto-approves when the compose worker's fidelity check passes — see
below) and the v3 `final_review` PR creation (the orchestrator dispatches the
submit worker directly, since the diff already passed `review_fix_global` +
`review_local` and a PR is editable and unmerged after creation). In both, the
compose worker still runs and the integrity anchor (fidelity check / drawer
immutability) is unchanged — only the human approval step is dropped.

### v3 bridge (PlanLocked → CodeImplementPending) — worker-owned

The PlanLocked bridge is worker-owned. The orchestrator does NOT call
`Skill('writing-plans')` inline, does NOT read verbose `final_plan`, and does
NOT build the `task_list` manifest. It dispatches `collab-turn-task-list.md`
twice:

- `$MODE=compose` — the worker invokes `writing-plans` to author the plan
  markdown at `docs/superpowers/plans/…`, derives the manifest, and runs a
  **fidelity check** against the markdown it just authored: (1) heading-count
  parity — count `^### Task ` headings and assert `manifest.tasks.length ==
  heading_count` and both counts are ≥ 1 (a zero count on either side is a
  failure — that is the `## Task` h2 mismatch); (2) task IDs are `1..N`
  contiguous; (3) every task has ≥1 `acceptance` entry. It returns
  `plan_file_path` + content hash + `fidelity:<pass|fail>` (with counts).
- **Auto-approve gate (fidelity-conditional)** — the COMPOSE→SUBMIT step is an
  LLM transcription of the locked plan into a manifest, not a deterministic
  parse, so silent under-counting (one dropped task, or the `## Task` vs
  `### Task` mismatch that has parsed 0 tasks in practice) is a real risk.
  `fidelity:pass` → the orchestrator auto-proceeds with no prompt and emits a
  ≤1-line audit note (`bridge auto-approved: N tasks, heading-parity OK`).
  `fidelity:fail` → the orchestrator falls back to the existing manual
  reference-only gate (surface path + hash + summary for approval).
- `$MODE=submit` (after auto-approve or manual approval) — the orchestrator
  passes `$ARTIFACT_REF=<approved plan_file_path>` and
  `$ARTIFACT_HASH=<approved hash>`; the same template rereads that plan file,
  recomputes its SHA-256 content hash, aborts with `failure_report` on
  mismatch, then parses it into the manifest and re-asserts heading-count parity
  as a defense-in-depth anchor. On a parity mismatch or zero tasks parsed it
  sends a `failure_report` instead of `task_list`.

Only refs/paths cross the orchestrator boundary; the plan markdown and the
manifest JSON never do.

### Measurement gate

The dispatch design targets **orchestrator context growth ≤ ~300 tokens per
protocol turn**, since the orchestrator ingests only a ≤3-line verdict per
worker and never the produced artifacts. This is measured via occupancy
sampling — the same metrics instrumentation tracked under #82–#83.

### Worker templates

The eight per-turn worker templates live under `.claude-plugin/prompts/`:

- `collab-turn-plan-draft.md` — `PlanParallelDrafts` blind draft
- `collab-turn-plan-synthesis.md` — `PlanSynthesisPending` canonical
- `collab-turn-plan-finalize.md` — `PlanClaudeFinalizePending` final
- `collab-turn-task-list.md` — `PlanLocked` bridge (`$MODE=compose|submit`)
- `collab-turn-code-implement.md` — `CodeImplementPending` batch (Claude implementer)
- `collab-turn-review-local.md` — `CodeReviewLocalPending` `/ultrareview-local` audit
- `collab-turn-final-review.md` — `CodeReviewFinalPending` PR-body compose
- `collab-turn-submit.md` — generic submit-by-ref + PR create

The Claude-side dispatch tables and the authoritative tier matrix live in
`.claude-plugin/commands/collab.md`; this section and that command file must
stay in lockstep (see the three-file header rule at the top of
`.codex-plugin/prompts/collab.md`).

## Autonomous Planning Loop

**Claude runs the single control loop.** Codex CLI sessions are one-shot:
each Codex-owned turn is dispatched inline via background `codex exec`
(see Implementation Notes § Background `codex exec` dispatch), reads
state, sends exactly one protocol message, and exits. There is no
symmetric long-running polling loop on the Codex side — Claude polls,
Claude dispatches. A single bounded `wait_my_turn` call at invocation
start (as in `.codex-plugin/prompts/collab.md`) is permitted to bridge
the brief server-write race after Claude's dispatch — that's a one-shot
boot-time wait, not a polling loop.

Claude's loop:

```text
loop:
  status = collab_status(session_id)
  if status.session_ended or status.phase in terminal_set: break
  if status.current_owner == "codex":
    dispatch Codex via background `codex exec`; poll until phase advances
    continue
  # current_owner == "claude"
  msgs = collab_recv(session_id, "claude", auto_ack=true)
  act on (status.phase, status.review_round) → send exactly one protocol message
```

Phase → action (v1):

| Phase | Claude does | Codex does |
|---|---|---|
| `PlanParallelDrafts` | send `draft` autonomously (no Plan Mode); owner flips to `codex` | one-shot bg-exec: send `draft`, exit |
| `PlanSynthesisPending` | `review_round == 0` → enter Plan Mode, get user approval, synthesize `canonical`, send. `review_round >= 1` → revise autonomously (no Plan Mode), send | wait (not polling) |
| `PlanCodexReviewPending` | wait | one-shot bg-exec: send `review` (or `approve` shortcut), exit |
| `PlanClaudeFinalizePending` | enter Plan Mode, get user approval, send `final` | wait |
| `PlanLocked` | exit loop (or send `task_list` to start v3) | n/a |

Phase → action (v3):

| Phase | Claude does | Codex does |
|---|---|---|
| `PlanLocked` (post-final) | dispatch `collab-turn-task-list.md` compose/submit workers; auto-approve when the compose fidelity check passes (manual ref gate only on fidelity-fail); worker sends `task_list` | n/a |
| `CodeImplementPending` (implementer=claude) | dispatch `collab-turn-code-implement.md`; worker searches checkpoints, runs `subagent-driven-development`, gates, checkpoints, and sends `implementation_done{head_sha}` | wait |
| `CodeImplementPending` (implementer=codex) | dispatch Codex via bg-exec; poll | one-shot bg-exec: search implementation checkpoints, resume/run `subagent-driven-development`, checkpoint every task boundary, emit `implementation_done{head_sha}`, exit |
| `CodeReviewFixGlobalPending` | dispatch Codex via bg-exec; poll | one-shot bg-exec: run `/pr-review-toolkit:review-pr` on the raw post-implementation diff, fix confirmed branch-level issues in place, send `review_fix_global`, exit |
| `CodeReviewLocalPending` | dispatch `collab-turn-review-local.md`; worker runs `/ultrareview-local`, fixes CRITICAL/HIGH/MEDIUM in place, and sends `review_local` | wait |
| `CodeReviewFinalPending` | dispatch `collab-turn-final-review.md` compose worker, then dispatch `collab-turn-submit.md` **directly** (no gate) to `gh pr create` (ready PR) and send `final_review{pr_url}` | wait |
| `CodingComplete` / `CodingFailed` | exit loop | n/a |

### Claude's Plan Mode Integration

Claude enters harness Plan Mode at **exactly two gates**, matching the
command-file invariant bullet:

1. **v1 first `canonical`** — `PlanSynthesisPending` with `review_round == 0`.
   The first artifact combining both drafts; the user's primary v1
   steering gate.
2. **v1 `final`** — `PlanClaudeFinalizePending`. The planning commit
   point; post-send the session is `PlanLocked`.

Both surviving gates approve plan *content*. Every step after plan-lock runs
autonomously: the blind `draft` send (from `/collab start`), revision-round
canonicals (`PlanSynthesisPending` with `review_round >= 1`), all Codex turns,
the v3 `task_list` send (auto-approved when the bridge fidelity check passes;
manual reference-only fallback on fail — never harness Plan Mode), and the v3
`final_review` PR creation (auto-dispatched after the diff passes
`review_fix_global` + `review_local`). Codex never enters Plan Mode — it posts
drafts, reviews, and global fixes directly.

## Prompt Templates

The user types the task; the agent fills in everything else. Normal path
is **single-terminal**: the user runs `/collab start` in Claude's terminal
and Claude dispatches every Codex turn inline via background `codex exec`.
The Codex-terminal "join" path is the fallback below.

### Starting a session (Claude's terminal — normal path)

User types:

```text
/collab start <one-sentence task>
```

or free-form:

```text
collab-start: <one-sentence task>
```

Claude's behavior on receiving this:

1. `repo_path` ← `git rev-parse --show-toplevel` of the current working directory.
2. `branch` ← `git branch --show-current`.
3. `initiator` ← `"claude"` (this is the Claude terminal).
4. `task` ← the text after `start`/`start:`.
5. Call `collab_start` with those four fields.
6. Report the returned `session_id` back to the user as a single-line
   tracking message (e.g. `Collab session started: <session_id>
   (implementer: <claude|codex>)`). Do not instruct the user to paste
   anything into a Codex terminal — Claude drives Codex inline via
   background `codex exec`.
7. Enter the autonomous planning loop as `claude` (see § Autonomous
   Planning Loop). Send the blind `draft` autonomously (no Plan Mode);
   enter Plan Mode only at the two gates listed in
   § Claude's Plan Mode Integration. Do not call `collab_end`.

### Joining a session in a Codex terminal — fallback only

This path is **only** used when both `codex` CLI on PATH and the
`mcp__codex__codex` MCP server are unavailable in Claude's terminal —
i.e., Claude has no way to dispatch Codex inline. The user manually
runs the join command in a separate Codex terminal so Codex can poll
for its own turn.

User types in a Codex terminal:

```text
/collab join <session_id>
```

or:

```text
collab-join <session_id>
```

Codex's behavior in this fallback:

1. Store `<session_id>` as the current session — every subsequent
   `collab_*` call uses it without re-prompting.
2. `agent` / `sender` / `receiver` ← `"codex"`.
3. Call `collab_status(session_id)` to read the task (the user
   does not re-type it on this side).
4. Run a one-shot turn for the current Codex-owned phase
   (`PlanParallelDrafts` draft, `PlanCodexReviewPending` review,
   `CodeReviewFixGlobalPending` global fix, or
   `CodeImplementPending` batch impl when `implementer == "codex"`).
   Codex CLI sessions are one-shot; the prompt exits after one send
   regardless. Do not call `collab_end`.

### Agent-side defaults — never ask the user

When the command does not specify these, the agent resolves them silently:

| Field | Source |
|---|---|
| `repo_path` | `git rev-parse --show-toplevel` |
| `branch` | `git branch --show-current` |
| `initiator` / `sender` / `receiver` / `agent` | `"claude"` in Claude's terminal, `"codex"` in Codex's |
| `session_id` (after first turn) | remembered from the start/join call |

If the agent is running somewhere without a git repo, it falls back to
`pwd` for `repo_path` and asks the user for a branch name.

## Worked Example

Single-terminal narrative (normal path). The user only types the
`/collab start` line; Claude drives every Codex turn inline.

```text
user (Claude terminal):
  /collab start design marketing landing page

Claude: resolves repo_path, branch, initiator=claude. start → s_abc.
        Draft sent autonomously — no Plan Mode (review_round == 0
        gates the first canonical, not the blind draft). Owner flips
        to codex.
Claude: dispatches Codex via background `codex exec` with the resolved
        Codex prompt. Begins polling collab_status + BashOutput.
Codex (bg-exec):
        reads status → task is "design marketing landing page".
        Submits one draft. Exits.
Claude: poll observes owner=claude, phase=PlanSynthesisPending,
        review_round=0. recv → sees Codex's draft. **Enters Plan Mode
        for the user-gated first canonical.** User approves. Sends
        canonical.
Claude: dispatches Codex again via bg-exec for the review.
Codex (bg-exec):
        reads canonical, returns verdict=request_changes. Exits.
Claude: poll observes phase=PlanSynthesisPending, review_round=1.
        **Revision-round canonical is autonomous — no Plan Mode.**
        Revises canonical incorporating Codex's notes, sends.
Claude: dispatches Codex again via bg-exec.
Codex (bg-exec):
        approve_with_minor_edits. Exits.
Claude: poll observes phase=PlanClaudeFinalizePending. **Enters Plan
        Mode for final.** User approves. Sends final. Phase now
        PlanLocked. Loop exits.
```

Two `request_changes` rounds would force Claude into
`PlanClaudeFinalizePending` without a third synthesis — last word is
still Claude's. Revision-round canonicals are always autonomous; only
the first canonical and the final are user-gated.

## Running the MCP Server

Trusted mode is required for collab writes:

```bash
IRONMEM_MCP_MODE=trusted ./target/release/ironmem serve
```

Smoke test without the embed model:

```bash
IRONMEM_MCP_MODE=trusted IRONMEM_EMBED_MODE=noop ./target/release/ironmem serve
```

## Implementation Notes

### Background `codex exec` dispatch (all Codex-owned phases)

The Claude dispatcher invokes ALL Codex-owned non-terminal phases via
`codex exec` as a background Bash process (`run_in_background: true`)
rather than via the synchronous `mcp__codex__codex` MCP tool. This
covers `PlanParallelDrafts`, `PlanCodexReviewPending`,
`CodeReviewFixGlobalPending`, and `CodeImplementPending+codex`. The full
procedure (prompt file selection, reasoning flag, polling loop, termination
conditions, and failure handling) is documented in the Claude-side
dispatcher prompt (`.claude-plugin/commands/collab.md`, section "Codex
handoff — background `codex exec`").

**Why all phases.** Background exec avoids the MCP cold-start overhead
that dominated latency in smoke testing (`PlanCodexReviewPending` hung
24+ min; `CodeReviewFixGlobalPending` took 171s via synchronous MCP).
The dispatch shape is now uniform across all Codex turns; only the prompt
file and the reasoning flag vary by phase. `CodeImplementPending+codex`
uses the slim `collab-batch-impl.md` prompt and
`-c model_reasoning_effort=xhigh`;
all other Codex turns use the full `collab.md` prompt with default reasoning
preserved (reviewer and planner judgment must not be shallow).

#### Fallback: synchronous `mcp__codex__codex` MCP

When `codex` is not on PATH, the dispatcher falls back to synchronous
`mcp__codex__codex` for any phase. The prompt-file selection matrix is
unchanged; only the transport differs.

1. Register `codex mcp-server` with Claude Code (once):
   ```bash
   claude mcp add codex codex mcp-server
   ```
2. Claude expands the Codex prompt locally — `codex mcp-server` does
   **not** resolve slash commands from `.codex-plugin/prompts/`, so
   passing a raw `/collab join <sid>` string would make Codex treat it
   as ordinary user text and go off-script. Read the appropriate
   prompt file (`.codex-plugin/prompts/collab.md` for plan/review
   phases; `.codex-plugin/prompts/collab-batch-impl.md` for
   `CodeImplementPending+codex`), substitute `$ARGUMENTS` with
   `join <session_id>`, and call:
   ```json
   {
     "name": "mcp__codex__codex",
     "arguments": {
       "prompt": "<resolved prompt text>",
       "cwd": "<repo_path>",
       "config": { "model_reasoning_effort": "xhigh" }
     }
   }
   ```
   The `config` block with `model_reasoning_effort: "xhigh"` is added
   **only** for `CodeImplementPending+codex`; all other phases omit
   `config` so reviewer and planner judgment stays at default depth.
   The call blocks until Codex finishes its phase-specific action and
   hands control back. Claude then resumes the dispatch loop.

This keeps the control loop inside Claude Code — no external daemon, no
FIFO, no turn-change webhook. If `mcp__codex__codex` is also not
registered, the prompt falls back to asking the user to run
`/collab join <session_id>` manually in a separate Codex terminal
(see § Prompt Templates — "Joining a session in a Codex terminal —
fallback only").

### Timing instrumentation (eval mode)

Claude writes one timing event per line to `/tmp/collab-eval-${session_id}.log`
at key transition points throughout the dispatcher. This is opt-in and
harmless: worst case a `/tmp` log file is written. Timing events never block
the protocol — if a write fails, swallow the error silently and continue.

**Rationale:** IronRace collab sessions span multiple agents and a long
batch-implementation phase. Post-run shell analysis of the log lets us
reconstruct the latency breakdown (planning vs. Codex dispatch vs. review vs.
PR), measure the background-exec speedup (A.2), and identify hangs.

**Format:** one event per line, with stable base names and structured
key=value metadata. The event name itself never embeds round or phase
detail; those go in `phase=` and `round=` fields:

```
<unix_seconds>.<nanos> <event_name> phase=<phase> round=<N> [<extra>]
```

Examples:

```
1778971814.91 t2_codex_dispatched phase=PlanCodexReviewPending round=2
1778971990.43 t3_codex_returned phase=PlanCodexReviewPending round=2
1778971814.93 t4_phase_advanced phase=PlanClaudeFinalizePending round=2
1778971990.99 t8_pr_created phase=CodeReviewFinalPending https://github.com/.../pull/123
```

**Required fields.** `phase=<phase>` and `round=<N>` are required on every
Codex-owned dispatch/return event (`t2_codex_dispatched`,
`t3_codex_returned`, `t6_codex_review_dispatched`,
`t7_codex_review_returned`) and on `t4_phase_advanced`. For
`t4_phase_advanced`, `phase=` is the new destination phase and `round=`
is the same dispatch round being watched by the polling loop (for
example, `round=2` for the second v1 review, `round=1` for the global
review phase). Events that fire exactly once per session
(`t0_session_started`, `t1_task_list_sent`, `t8_pr_created`,
`t9_final_review_sent`, `t10_session_complete`) MAY omit `round=`; they
retain `phase=` where meaningful. Suffixed event-name shapes that bake
the round number or destination phase into the identifier are legacy
artifacts — do not emit them.

**Write an event:**
```bash
echo "$(date +%s.%N) <event_name> phase=<phase> round=<N> [<extra>]" >> /tmp/collab-eval-${session_id}.log
```

**Event list:**

| Event | Required fields | When to write |
|---|---|---|
| `t0_session_started` | (none required) | Right after `collab_start` returns (session_id now known) |
| `t1_task_list_sent` | (none required) | Right after `collab_send(topic="task_list")` returns |
| `t2_codex_dispatched` | `phase=` `round=` | Immediately before launching background `codex exec` for any Codex-owned phase (PlanParallelDrafts, PlanCodexReviewPending, CodeImplementPending+codex) |
| `t2_fallback_to_mcp` | `phase=` | When `codex` is not on PATH and falling back to synchronous MCP (any phase) |
| `t3_codex_returned` | `phase=` `round=` | Immediately after the bg-exec polling loop exits successfully for PlanParallelDrafts, PlanCodexReviewPending, or CodeImplementPending+codex |
| `t4_phase_advanced` | `phase=` `round=` | Every time a poll observes a new phase (destination phase goes in `phase=`, NOT in the event name; round is the same dispatch round being watched by the polling loop) |
| `t5_review_local_sent` | `phase=CodeReviewLocalPending` | After `collab_send(topic="review_local")` returns |
| `t6_codex_review_dispatched` | `phase=` `round=` | Immediately before launching background `codex exec` for `CodeReviewFixGlobalPending` |
| `t7_codex_review_returned` | `phase=` `round=` | Immediately after the bg-exec polling loop exits successfully for `CodeReviewFixGlobalPending` |
| `t8_pr_created` | `phase=CodeReviewFinalPending` | After `gh pr create` returns success; include the PR URL as `[extra]` |
| `t9_final_review_sent` | `phase=CodeReviewFinalPending` | After `collab_send(topic="final_review")` returns |
| `t10_session_complete` | `phase=` (CodingComplete or CodingFailed) | When `collab_status.phase` first reads `CodingComplete` or `CodingFailed` |

**Renamed event.** The old phase-advance event (one event per destination
phase, with the destination baked into the name) is now a single event,
`t4_phase_advanced`, with the destination phase carried in the `phase=`
field. There is one event name; the phase is data, not part of the
identifier. This makes log filtering and aggregation grep-stable across
phases.

**Legacy/incorrect forms.** Any event name that bakes the round number or
destination phase into the identifier (an `_round<N>` suffix on a Codex
dispatch/return event, or an `_to_<phase>` suffix on the phase-advance
event) is a legacy artifact of earlier runs. Those shapes are NOT
canonical and must not be emitted by current dispatchers. Historical
logs containing them are not rewritten; new logs use the structured form
above.

**Analyze post-run:**
```bash
# Show all events for a session with human-readable timestamps
session_id="<sid>"
awk '{printf "%s %s %s\n", strftime("%H:%M:%S", $1), $2, $3}' \
  /tmp/collab-eval-${session_id}.log

# Compute elapsed time between t0 and t10
grep -E "t0_session_started|t10_session_complete" \
  /tmp/collab-eval-${session_id}.log | awk 'NR==1{s=$1} NR==2{printf "Total: %.1fs\n", $1-s}'
```

Named events span `t0_session_started` (after `collab_start` returns)
through `t10_session_complete` (when `CodingComplete` or `CodingFailed`
is first observed).

### Polling backoff (Codex bg phases only)

The Claude dispatcher polls `collab_status` + `BashOutput` on a fixed ~10s
interval while a Codex-owned phase is running as a background
`codex exec` process. For long silent grinds, that fixed cadence churns
the dispatcher without producing new information. A bounded backoff curve
reduces that churn:

- Default poll interval: **10s**.
- After **60s** of no progress (no `phase` advance AND no new stdout
  line) → escalate to **20s**.
- After **300s** (5 min) of no progress → escalate to **30s** (cap).
- **Reset to 10s** on ANY of: phase advance, new stdout line, bg process
  exit, bg process error/signal.
- 600s hang detection is unchanged.

Scope: this backoff applies **only** to Codex-owned background phases
(`PlanParallelDrafts`, `PlanCodexReviewPending`,
`CodeReviewFixGlobalPending`, `CodeImplementPending+codex`). It does NOT
change the user-visible idle gap during Claude's Plan Mode prompts —
those are gated on user input, not on poll cadence. Full curve + reset
conditions are documented in the Claude-side dispatcher prompt.

### Anti-removal: `/ultrareview-local` overlap audit

Under the new v3 ordering, `CodeReviewLocalPending` runs after Codex's
`review_fix_global`. Its role is **audit of Codex's commits** plus
catching code-quality, maintainability, consistency, and local-read
issues that Codex's pr-review-toolkit-backed branch review may miss.
`/ultrareview-local` (code-reviewer + security-reviewer +
architect + doc-reviewer in parallel, synthesized inline) exercises a
different set of agent prompts and a different synthesis path than
Codex's review toolkit pass.

Removing the `CodeReviewLocalPending` stage from the v3 flow therefore
requires a written **overlap audit**: a demonstration, against a
representative sample of prior collab sessions, that Codex's
`pr-review-toolkit`-backed `review_fix_global` reviews catch the
code-quality issues `/ultrareview-local` would have flagged AND that the
audit-of-Codex role is unnecessary (e.g., Codex's commits never
reintroduce issues).
Without that audit, the stage stays.

**Status as of 2026-05-27: kept.** Code-quality / consistency overlap
with Codex's branch review is accepted as deliberate. Removal still
requires the written overlap audit specified above.

### SDD reviewer model pinning (out of protocol)

The `subagent-driven-development` skill's reviewer prompts in this repo
do not currently pin a model — reviewer subagents run on the parent
session's default. Recommendations to "use Haiku for reviewers" or
similar model-selection rules require skill-side pinning support that
does not exist in the current prompt surface. If preferred-model
guidance becomes load-bearing later, it should live in the SDD skill
itself **after** model pinning exists — NOT in the collab protocol
spec. The collab protocol is intentionally model-agnostic for
subagent-driven implementation.

## Code-Map MCP Tools (issue #94)

Worker turns use three MCP tools to lazily cache and retrieve per-area code
maps, reducing redundant file-reading across turns.

### `code_map_write`

Persists a code map for a named area of the repository.

```json
{
  "repo": "/absolute/path/to/repo",
  "area": "src/collab",
  "head_sha": "a1b2c3d4e5f6...",
  "source_files": ["src/collab/mod.rs", "src/collab/state.rs"],
  "summary": "...",
  "built_by": "scout-worker",
  "turn_id": "task-3"
}
```

**Required fields:** `repo`, `area`, `head_sha`, `source_files` (non-empty),
`summary`, `built_by`. `turn_id` is optional (used for metrics attribution —
see below).

**Input validation:**
- `repo` must be an existing git worktree — an absolute path (no `..`
  traversal), canonicalized and resolved to its `git rev-parse --show-toplevel`
  before storage.
- `head_sha` must be a hex git object name (7–64 hex chars; the HEAD the map was
  built at). A non-hex value is rejected at write time.
- Each entry in `source_files` must be repo-relative and forward-slash
  normalized: no leading `/`, no `..` components, no backslash or NUL; the set
  must be non-empty.

Returns `{ "success": true, "drawer_id": ..., "wing": <repo>, "room": "code-maps" }`.

### `code_map_load`

Retrieves the stored code map for an area and returns its freshness status.

```json
{
  "repo": "/absolute/path/to/repo",
  "area": "src/collab",
  "turn_id": "task-3"
}
```

The response carries `found` (bool) and a `freshness` object whose `verdict`
field is one of:
- `fresh` — the map was built at the current `HEAD`. Use it as a
  WHERE-to-look pointer; verify load-bearing details against actual source.
- `stale` — `HEAD` has advanced since the map was built. The `freshness`
  object includes a `changed_files` list. Re-read only those files, then call
  `code_map_write` to refresh before proceeding.
- `rescout_required` — no map exists (or it cannot be verified). Scout the
  area and call `code_map_write` to create one.

### `code_map_status`

Returns the freshness status and metadata for a stored map without loading
its full content. Useful for a quick pre-flight check before deciding whether
to load or re-scout.

```json
{ "repo": "/absolute/path/to/repo", "area": "src/collab" }
```

### Lazy first-touch worker flow

```
code_map_load(repo, area, turn_id)
  ├─ fresh             → use map as exploration pointer; verify details in code
  ├─ stale             → re-read changed_files; code_map_write to refresh; proceed
  └─ rescout_required  → scout area (read files, trace paths); code_map_write; proceed
     / not found
```

**Re-verify caveat:** Maps are WHERE-to-look pointers, not authoritative
documentation. Before relying on any load-bearing detail — function
signatures, type invariants, call-site counts — re-verify against the actual
source. Never treat a map entry alone as a contract-level claim.

### Exploration-token attribution

Each `code_map_load` and `code_map_write` call emits a `source='mcp_response'`
`token_usage` row with `map_status`, `turn_id`, and `area` set. `map_status` is
`map_hit` **only** when `code_map_load` returns a found map with verdict
`fresh`; every other case — a `stale` load, a `rescout_required`/absent load,
and every `code_map_write` — is tagged `map_miss`. `code_map_status` does NOT
emit an attribution row (it is a metadata-only pre-flight check, not an
exploration call). `turn_id` is read from the top-level MCP request arguments
by the server layer, so callers must pass it as a top-level argument. See
`docs/METRICS_SPEC.md` — Amendment v11 for the full schema and reporting spec.

## Validation

```bash
cargo test -p ironmem collab::
cargo test -p ironmem --test mcp_protocol
cargo test -p ironmem
cargo clippy -p ironmem -- -D warnings
```

Tool-surface smoke test:

```bash
cargo build -p ironmem --release
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  | env HOME=/tmp/ironmem-home IRONMEM_EMBED_MODE=noop IRONMEM_MCP_MODE=trusted \
      ./target/release/ironmem serve --db /tmp/ironmem-collab-tools.sqlite3 \
  | python3 -c "import sys,json; t=[x['name'] for x in json.load(sys.stdin)['result']['tools']]; \
      assert all(f'collab_{n}' in t for n in ['start','send','recv','ack','status','approve','register_caps','get_caps','wait_my_turn','end']), t; \
      assert 'session_handoff' in t, t; print('OK')"
```

## Scope and Limits

Scope (v1 + v3):

- bounded planning (v1) and bounded coding loop (v3) through a single session
- one plan → one task list → one PR per session
- v1 planning is 2 review rounds; v3 coding is strictly linear (no rounds)
- Claude always gets the last word in planning (v1) and owns the
  audit/PR turns after Codex's first branch-scope review in v3
- Claude runs the dispatcher loop; Codex-owned phases are one-shot
  dispatches that act autonomously and exit

Out of scope:

- multi-session orchestration
- parallel branches / concurrent PRs
- autonomous merge (Claude opens the PR; a human merges)
