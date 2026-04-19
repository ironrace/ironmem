# IronRace Collab (v1 Planning + v2 Coding)

`ironmem` includes a bounded collaboration protocol that lets Claude Code
and Codex coordinate a single plan and then implement it through the shared
MCP server.

- **v1 (planning)**: bounded parallel drafts → canonical synthesis → Codex
  review → Claude finalize → `PlanLocked`.
- **v2 (coding)**: post-`PlanLocked` task list → per-task 5-phase debate →
  local review → 2-pass global Codex review → PR handoff →
  `CodingComplete` / `CodingFailed`.

This document covers:

- the full state machine and invariants (v1 + v2)
- the `collab_*` MCP tools
- topic payload formats for every protocol message
- harness-side responsibilities (git, cargo, gh, coderabbit)
- the autonomous long-poll loop each agent runs
- Claude's Plan Mode integration for canonical synthesis and revisions
- copy-pasteable prompts for the Claude and Codex terminals
- a worked example

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

## Runtime Model

```text
Claude / Codex (each in its own terminal / worktree)
  └─ collab_* MCP tools
      └─ ironmem serve (stdio)
          └─ SQLite (sessions, messages, capabilities, wal_log)
```

Protocol enforcement lives in the server. The agents are thin clients that
long-poll `wait_my_turn` and react to the state machine.

## Session State

Stored in `collab_sessions`:

| Field | Meaning |
|---|---|
| `id` | Session identifier (returned from `collab_start`) |
| `repo_path`, `branch` | Where this plan applies |
| `task` | Human description of the planning goal. Set at `start`, readable via `status`. |
| `phase` | Current protocol phase (see below) |
| `current_owner` | Agent whose turn it is (`claude` or `codex`) |
| `claude_draft_hash`, `codex_draft_hash` | SHA-256 of each first draft |
| `canonical_plan_hash` | SHA-256 of Claude's synthesis |
| `final_plan_hash` | SHA-256 of the locked plan |
| `codex_review_verdict` | Last Codex verdict |
| `review_round` | Number of completed Codex reviews (0, 1, or 2) |
| `ended_at` | Non-null once `collab_end` has been called |

All state changes are recorded in `wal_log`.

## Phase Model

### `PlanParallelDrafts`

Both agents submit exactly one `draft`. Order is not enforced.

**Blind-draft invariant:** `collab_recv` suppresses a counterpart's
`draft` until the calling agent has submitted its own. This is enforced
server-side, not by convention.

Exit: once both draft hashes are present → `PlanSynthesisPending`, owner
`claude`.

### `PlanSynthesisPending`

Owner: `claude`. Claude sends one `canonical` message containing the merged
plan.

This phase is also re-entered on `request_changes`, so Claude uses it both
for the first synthesis and for revisions.

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
- `collab_send` with `topic=task_list` from `claude` — enter the v2 coding
  loop. The state machine verifies `plan_hash == final_plan_hash` and the
  task list is non-empty; the session stays active and the terminal set for
  `wait_my_turn` flips to `{CodingComplete, CodingFailed}`.

## v2 Coding Phase Model

v2 reuses the same session (no new `id`). It extends `collab_sessions` with
per-task and global round counters, a `base_sha` / `last_head_sha` pair for
branch-drift detection, `pr_url` for the PR handoff, and `coding_failure`
for unrecoverable errors. Each phase names the exact event that advances it.

### Per-task 5-phase debate

Applied once per task in `task_list`. `task_review_round` counts Codex-review
passes; at `MAX_TASK_REVIEW_ROUNDS = 2`, `verdict=disagree_with_reasons`
skips `CodeDebatePending` and lands directly in `CodeFinalPending`, which
**advances the task** instead of looping back.

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeImplementPending` | `claude` | `CodeImplement{head_sha}` | `CodeReviewPending` |
| `CodeReviewPending` | `codex` | `CodeReview{head_sha}` | `CodeVerdictPending` |
| `CodeVerdictPending` | `claude` | `CodeVerdict{verdict, head_sha}` — `agree` advances task; `disagree_with_reasons` under cap → `CodeDebatePending`; at cap → `CodeFinalPending` | varies |
| `CodeDebatePending` | `codex` | `CodeComment{head_sha}` | `CodeFinalPending` |
| `CodeFinalPending` | `claude` | `CodeFinal{head_sha}` — under cap loops to `CodeReviewPending`; at cap advances task | varies |

**Advance**: `task_review_round` resets to 0; if another task remains,
`current_task_index += 1` and phase returns to `CodeImplementPending`,
else phase transitions to `CodeReviewLocalPending`.

### Local + global review

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeReviewLocalPending` | `claude` | `ReviewLocal{head_sha}` | `CodeReviewCodexPending` |
| `CodeReviewCodexPending` | `codex` | `ReviewGlobal{verdict, head_sha}` — `agree` → `PrReadyPending`; `disagree_with_reasons` → `CodeReviewVerdictPending` (bumps `global_review_round`) | varies |
| `CodeReviewVerdictPending` | `claude` | `VerdictGlobal{verdict, head_sha}` | `CodeReviewDebatePending` |
| `CodeReviewDebatePending` | `codex` | `CommentGlobal{head_sha}` | `CodeReviewFinalPending` |
| `CodeReviewFinalPending` | `claude` | `FinalReview{head_sha}` — under `MAX_GLOBAL_REVIEW_ROUNDS = 2` loops to `CodeReviewCodexPending`; at cap forces `PrReadyPending` | varies |

### PR handoff + terminal

| Phase | Owner | Event | Next |
|---|---|---|---|
| `PrReadyPending` | `claude` | `PrOpened{pr_url, head_sha}` | `CodingComplete` (terminal) |
| *any coding-active phase* | either | `FailureReport{coding_failure}` | `CodingFailed` (terminal) |

`collab_end` is **rejected** in every coding-active phase
(`CodeImplementPending` through `PrReadyPending`). Only `CodingComplete` or
`CodingFailed` end the session post-`task_list`.

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
  "task": "design the marketing landing page"
}
```

Returns `{ session_id, task }`. The `task` is stored on the session so the
counterpart agent can read it via `collab_status` without a manual
paste.

### `collab_send`

Sends a protocol message and advances the state machine.

```json
{ "session_id": "...", "sender": "claude", "topic": "draft", "content": "..." }
```

v1 planning topics: `draft`, `canonical`, `review`, `final`.

v2 coding topics: `task_list`, `implement`, `verdict`, `comment`,
`review_local`, `review_global`, `verdict_global`, `comment_global`,
`final_review`, `pr_opened`, `failure_report`.

The phase→topic acceptance matrix is tabulated in
[§ Phase → Topic Acceptance](#phase--topic-acceptance); consult that table
before every `collab_send`.

### `collab_recv`

Returns pending messages. Enforces the blind-draft invariant.

### `collab_ack`

Marks a message consumed. Session-scoped: a mismatched
`(session_id, message_id)` pair is rejected.

### `collab_status`

Returns the full session record including `phase`, `current_owner`, `task`,
`review_round`, `ended_at`, and all hashes. Call this before every protocol
action.

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
through `PrReadyPending`). This prevents either agent from killing a session
the counterpart is still working in.

Idempotent once allowed: calling from a terminal phase or an
already-ended session is a no-op, and subsequent `send`, `ack`, `approve`,
`register_caps`, and `wait_my_turn` calls all treat the session as ended.

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

### v2 coding topic payloads

Every v2 `collab_send` content is JSON. The server parses strictly — missing
or empty required fields reject with a validation error. `head_sha` appears
on nearly every coding message so the server can record branch progress and
either agent can detect drift.

| Topic | Sender | Payload | Notes |
|---|---|---|---|
| `task_list` | `claude` | `{"plan_hash","base_sha","head_sha","tasks":[{"id","title","acceptance":[...]}]}` | `plan_hash` must equal `final_plan_hash`; `tasks` must be non-empty and strictly ordered by `id`; each task requires ≥1 `acceptance` entry. |
| `implement` | `claude` | `{"head_sha"}` | Harness has pushed the commit before sending. |
| `review` (v2) | `codex` | `{"head_sha"}` | In `CodeReviewPending` only. |
| `verdict` | `claude` | `{"head_sha","verdict":"agree"\|"disagree_with_reasons"}` | |
| `comment` | `codex` | `{"head_sha"}` | In `CodeDebatePending`. Full rebuttal lives in the content (free text alongside `head_sha`). |
| `final` (v2) | `claude` | `{"head_sha"}` | In `CodeFinalPending` only. |
| `review_local` | `claude` | `{"head_sha"}` | |
| `review_global` | `codex` | `{"head_sha","verdict":"agree"\|"disagree_with_reasons"}` | |
| `verdict_global` | `claude` | `{"head_sha","verdict":...}` | |
| `comment_global` | `codex` | `{"head_sha"}` | |
| `final_review` | `claude` | `{"head_sha"}` | |
| `pr_opened` | `claude` | `{"head_sha","pr_url"}` | |
| `failure_report` | either | `{"coding_failure":"<reason>"}` | Valid in any coding-active phase. |

### Phase → Topic Acceptance

The server dispatches strictly on the current phase. The topic strings
`review` and `final` are overloaded — their semantics depend on the phase
at the time of `collab_send`. All other topics map 1:1.

| Phase | Accepted topic(s) | Notes |
|---|---|---|
| `PlanParallelDrafts` | `draft` | v1 planning |
| `PlanSynthesisPending` | `canonical` | v1 planning |
| `PlanReviewPending` | `review` | v1 — Codex review of canonical |
| `PlanClaudeFinalizePending` | `final` | v1 — Claude finalizes |
| `PlanLocked` | `task_list` | v1 → v2 hand-off |
| `CodeImplementPending` | `implement`, `failure_report` | |
| `CodeReviewPending` | `review`, `failure_report` | **v2** review, not v1 |
| `CodeVerdictPending` | `verdict`, `failure_report` | |
| `CodeDebatePending` | `comment`, `failure_report` | |
| `CodeFinalPending` | `final`, `failure_report` | **v2** final, not v1 |
| `CodeReviewLocalPending` | `review_local`, `failure_report` | |
| `CodeReviewCodexPending` | `review_global`, `failure_report` | |
| `CodeReviewVerdictPending` | `verdict_global`, `failure_report` | |
| `CodeReviewDebatePending` | `comment_global`, `failure_report` | |
| `CodeReviewFinalPending` | `final_review`, `failure_report` | |
| `PrReadyPending` | `pr_opened`, `failure_report` | |
| `CodingComplete` / `CodingFailed` | *(none — terminal; only `collab_end` accepted)* | |

`failure_report` is accepted from either agent in any coding-active phase
and transitions the session to `CodingFailed`. All other topics are gated
by the owner recorded in the phase table above.

## Harness-Side Responsibilities

The server is pure: it validates transitions, persists hashes, and routes
messages. Every shell-level action — git, cargo, gh, coderabbit — is the
**agent harness's** responsibility. The protocol relies on the harness doing
these things between `wait_my_turn` and `collab_send`:

- **`base_sha` / `head_sha` tracking.** The harness records `base_sha` at
  `task_list` send time (the commit the branch forked from) and the current
  `head_sha` on every subsequent send. Before acting on an incoming turn,
  the harness reads `last_head_sha` from `collab_status` and runs
  `git cat-file -e <sha>^{commit}` to verify the commit is present; if not,
  it sends `failure_report` with `coding_failure: "branch_drift: ..."`.
- **Local gates** before `implement` / `final` / `review_local`:
  `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`.
  Any failure surfaces as `failure_report`; don't try to hide it.
- **Review tooling** during Codex's `review` and `review_global`:
  `coderabbit` (or equivalent) plus manual review. Verdicts ride on the
  `verdict` / `review_global` payloads.
- **PR creation** during `pr_opened`: `gh pr create --base <base_sha> ...`;
  capture the URL and include it in the send.
- **Plan Mode** on Claude's side is entered before `canonical`, `final` (v1),
  and `final` (v2). Codex never enters Plan Mode.

The server does not shell out, does not read the git tree, and does not
verify a commit exists — it trusts the harness's `head_sha` string. Drift
detection is therefore cooperative: either agent can raise `failure_report`
as soon as their local verification fails.

## Autonomous Planning Loop

Each agent runs the same shape of loop:

```text
loop:
  wait = collab_wait_my_turn(session_id, me, 30)
  if wait.session_ended or wait.phase == "PlanLocked": break
  if not wait.is_my_turn: continue

  status = collab_status(session_id)
  msgs   = collab_recv(session_id, me)
  for m in msgs: collab_ack(session_id, m.id)

  act on (status.phase, status.current_owner) → send exactly one protocol message
```

Phase → action (v1):

| Phase | Claude does | Codex does |
|---|---|---|
| `PlanParallelDrafts` | send `draft` (once) | send `draft` (once) |
| `PlanSynthesisPending` | enter Plan Mode, synthesize `canonical`, send | wait |
| `PlanCodexReviewPending` | wait | send `review` (or `approve` shortcut) |
| `PlanClaudeFinalizePending` | enter Plan Mode, send `final` | wait |
| `PlanLocked` | exit loop (or send `task_list` to start v2) | exit loop |

Phase → action (v2):

| Phase | Claude does | Codex does |
|---|---|---|
| `PlanLocked` (post-final) | verify `base_sha`, build `task_list` JSON, send | wait |
| `CodeImplementPending` | run gates, commit, push, send `implement` | wait |
| `CodeReviewPending` | wait | run reviewer tooling, send `review` |
| `CodeVerdictPending` | send `verdict` (`agree` or `disagree_with_reasons`) | wait |
| `CodeDebatePending` | wait | send `comment` with rebuttal |
| `CodeFinalPending` | apply fixes, re-run gates, send `final` | wait |
| `CodeReviewLocalPending` | run gates once more, send `review_local` | wait |
| `CodeReviewCodexPending` | wait | run coderabbit, send `review_global` |
| `CodeReviewVerdictPending` | send `verdict_global` | wait |
| `CodeReviewDebatePending` | wait | send `comment_global` |
| `CodeReviewFinalPending` | send `final_review` | wait |
| `PrReadyPending` | `gh pr create`, send `pr_opened` | wait |
| `CodingComplete` / `CodingFailed` | exit loop | exit loop |

### Claude's Plan Mode Integration

Claude enters Plan Mode **only** before sending `final`. Everything
before that — the initial draft, the canonical synthesis, and any
revision rounds — runs autonomously without interrupting the user.

The user is gated once, at the finalize step, because that's the commit
point: after `final` lands the session is `PlanLocked`. Codex does not
use Plan Mode — it posts drafts and reviews directly.

## Prompt Templates

The user types the task; the agent fills in everything else.

### Starting a session (Claude's terminal)

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
6. Report the returned `session_id` back to the user in a format they can
   paste into Codex's terminal verbatim, e.g.
   `collab-join <session_id>`.
7. Enter the autonomous planning loop as `claude`:
   `wait_my_turn → status → recv/ack → act`. Enter Plan Mode before
   sending `canonical` or `final`. Do not call `collab_end`.

### Joining a session (Codex's terminal)

User types:

```text
/collab join <session_id>
```

or:

```text
collab-join <session_id>
```

Codex's behavior:

1. Store `<session_id>` as the current session — every subsequent
   `collab_*` call uses it without re-prompting.
2. `agent` / `sender` / `receiver` ← `"codex"` (this is the Codex terminal).
3. Call `collab_status(session_id)` to read the task (the user
   does not re-type it on this side).
4. Enter the autonomous planning loop as `codex`:
   `wait_my_turn → status → recv/ack → act`. One draft, then up to two
   reviews. Claude has the last word. Do not call `collab_end`.

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

```text
user (Claude terminal):
  /collab start design marketing landing page

Claude: resolves repo_path, branch, initiator=claude. start → s_abc.
        draft sent. wait_my_turn → codex owns.
        Tells the user: "Run in Codex: collab-join s_abc"

user (Codex terminal):
  collab-join s_abc

Codex:  status → task is "design marketing landing page". draft sent.
Claude: wait_my_turn fires → owner=claude, phase=PlanSynthesisPending.
        recv → sees Codex's draft. Enter Plan Mode. canonical sent.
Codex:  wait_my_turn fires → codex owns. review verdict=request_changes.
Claude: wait_my_turn fires → phase=PlanSynthesisPending (round 1 done).
        revise canonical in Plan Mode. send canonical.
Codex:  approve_with_minor_edits.
Claude: wait_my_turn fires → PlanClaudeFinalizePending. send final.
        Status now PlanLocked. Loop exits.
```

Two rounds of `request_changes` would force Claude into
`PlanClaudeFinalizePending` without another synthesis — last word is still
Claude's.

## Running the MCP Server

Trusted mode is required for collab writes:

```bash
IRONMEM_MCP_MODE=trusted ./target/release/ironmem serve
```

Smoke test without the embed model:

```bash
IRONMEM_MCP_MODE=trusted IRONMEM_EMBED_MODE=noop ./target/release/ironmem serve
```

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
      assert all(f'collab_{n}' in t for n in ['start','send','recv','ack','status','approve','register_caps','get_caps','wait_my_turn','end']), t; print('OK')"
```

## Scope and Limits

Scope (v1 + v2):

- bounded planning (v1) and bounded coding loop (v2) through a single session
- one plan → one task list → one PR per session
- Claude always gets the last word in both the planning and per-task debates
- round caps force advance; no indefinite ping-pong
- long-poll via `wait_my_turn`; agents run autonomously

Out of scope:

- multi-session orchestration
- parallel branches / concurrent PRs
- autonomous merge (Claude opens the PR; a human merges)
