# IronRace Collab (v1 — Bounded Planning)

`ironrace-memory` includes a bounded planning protocol that lets Claude Code
and Codex coordinate a single plan through the shared MCP server.

v1 covers the **planning stage only**. The implementation stage will be added
in v2.

This document covers:

- the state machine and invariants
- the 10 `ironmem_collab_*` MCP tools
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
  └─ ironmem_collab_* MCP tools
      └─ ironmem serve (stdio)
          └─ SQLite (sessions, messages, capabilities, wal_log)
```

Protocol enforcement lives in the server. The agents are thin clients that
long-poll `wait_my_turn` and react to the state machine.

## Session State

Stored in `collab_sessions`:

| Field | Meaning |
|---|---|
| `id` | Session identifier (returned from `ironmem_collab_start`) |
| `repo_path`, `branch` | Where this plan applies |
| `task` | Human description of the planning goal. Set at `start`, readable via `status`. |
| `phase` | Current protocol phase (see below) |
| `current_owner` | Agent whose turn it is (`claude` or `codex`) |
| `claude_draft_hash`, `codex_draft_hash` | SHA-256 of each first draft |
| `canonical_plan_hash` | SHA-256 of Claude's synthesis |
| `final_plan_hash` | SHA-256 of the locked plan |
| `codex_review_verdict` | Last Codex verdict |
| `review_round` | Number of completed Codex reviews (0, 1, or 2) |
| `ended_at` | Non-null once `ironmem_collab_end` has been called |

All state changes are recorded in `wal_log`.

## Phase Model

### `PlanParallelDrafts`

Both agents submit exactly one `draft`. Order is not enforced.

**Blind-draft invariant:** `ironmem_collab_recv` suppresses a counterpart's
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

Terminal. The plan is frozen. `final_plan_hash` is set.

Do **not** call `ironmem_collab_end` at this point during v1 — the session
stays alive and will be consumed by the v2 coding phase once that ships.

## Blind-Draft Invariant

During `PlanParallelDrafts`, neither agent can see the other's draft until
it has submitted its own. This prevents drift toward the first draft that
lands.

Enforcement: `ironmem_collab_recv` filters out `draft` topic messages from
the counterpart whenever the caller has not yet submitted its own draft.

## MCP Tools

### `ironmem_collab_start`

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
counterpart agent can read it via `ironmem_collab_status` without a manual
paste.

### `ironmem_collab_send`

Sends a protocol message and advances the state machine.

```json
{ "session_id": "...", "sender": "claude", "topic": "draft", "content": "..." }
```

Protocol topics: `draft`, `canonical`, `review`, `final`. Any other topic is
rejected.

### `ironmem_collab_recv`

Returns pending messages. Enforces the blind-draft invariant.

### `ironmem_collab_ack`

Marks a message consumed. Session-scoped: a mismatched
`(session_id, message_id)` pair is rejected.

### `ironmem_collab_status`

Returns the full session record including `phase`, `current_owner`, `task`,
`review_round`, `ended_at`, and all hashes. Call this before every protocol
action.

### `ironmem_collab_approve`

Codex-only shortcut for an `approve` review. Requires `content_hash` to
match the stored `canonical_plan_hash`.

### `ironmem_collab_wait_my_turn` (long-poll)

Blocks server-side until the caller is the owner, the session ends, the
phase becomes terminal (`PlanLocked`), or `timeout_secs` elapses.

```json
{ "session_id": "...", "agent": "claude", "timeout_secs": 30 }
```

Returns `{ is_my_turn, phase, current_owner, session_ended }`. Default
timeout 30s, max 60s. Agents loop on this instead of polling `status` on a
fixed interval.

### `ironmem_collab_register_caps` / `ironmem_collab_get_caps`

Advisory: each agent registers available sub-agents/tools so the other can
plan around them.

### `ironmem_collab_end`

Idempotently ends a session. Sets `ended_at`; subsequent `send`, `ack`,
`approve`, `register_caps`, and `wait_my_turn` calls are rejected.

**Reserved for v2.** Do not call during planning. It is shipped now so the
v2 coding phase can use it without another migration.

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

## Autonomous Planning Loop

Each agent runs the same shape of loop:

```text
loop:
  wait = ironmem_collab_wait_my_turn(session_id, me, 30)
  if wait.session_ended or wait.phase == "PlanLocked": break
  if not wait.is_my_turn: continue

  status = ironmem_collab_status(session_id)
  msgs   = ironmem_collab_recv(session_id, me)
  for m in msgs: ironmem_collab_ack(session_id, m.id)

  act on (status.phase, status.current_owner) → send exactly one protocol message
```

Phase → action:

| Phase | Claude does | Codex does |
|---|---|---|
| `PlanParallelDrafts` | send `draft` (once) | send `draft` (once) |
| `PlanSynthesisPending` | enter Plan Mode, synthesize `canonical`, send | wait |
| `PlanCodexReviewPending` | wait | send `review` (or `approve` shortcut) |
| `PlanClaudeFinalizePending` | enter Plan Mode, send `final` | wait |
| `PlanLocked` | exit loop | exit loop |

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
5. Call `ironmem_collab_start` with those four fields.
6. Report the returned `session_id` back to the user in a format they can
   paste into Codex's terminal verbatim, e.g.
   `collab-join <session_id>`.
7. Enter the autonomous planning loop as `claude`:
   `wait_my_turn → status → recv/ack → act`. Enter Plan Mode before
   sending `canonical` or `final`. Do not call `ironmem_collab_end`.

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
   `ironmem_collab_*` call uses it without re-prompting.
2. `agent` / `sender` / `receiver` ← `"codex"` (this is the Codex terminal).
3. Call `ironmem_collab_status(session_id)` to read the task (the user
   does not re-type it on this side).
4. Enter the autonomous planning loop as `codex`:
   `wait_my_turn → status → recv/ack → act`. One draft, then up to two
   reviews. Claude has the last word. Do not call `ironmem_collab_end`.

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
cargo test -p ironrace-memory collab::
cargo test -p ironrace-memory --test mcp_protocol
cargo test -p ironrace-memory
cargo clippy -p ironrace-memory -- -D warnings
```

Tool-surface smoke test:

```bash
cargo build -p ironrace-memory --release
echo '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}' \
  | env HOME=/tmp/ironmem-home IRONMEM_EMBED_MODE=noop IRONMEM_MCP_MODE=trusted \
      ./target/release/ironmem serve --db /tmp/ironmem-collab-tools.sqlite3 \
  | python3 -c "import sys,json; t=[x['name'] for x in json.load(sys.stdin)['result']['tools']]; \
      assert all(f'ironmem_collab_{n}' in t for n in ['start','send','recv','ack','status','approve','register_caps','get_caps','wait_my_turn','end']), t; print('OK')"
```

## Scope and Limits

v1 scope:

- bounded planning only
- one plan per session
- Claude always gets the last word (no escalate state)
- long-poll via `wait_my_turn`; agents run autonomously

Out of scope for v1:

- the coding loop (v2)
- PR creation / review
- multi-session orchestration
