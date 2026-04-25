# IronRace Collab (v1 Planning + v3 Coding)

`ironmem` includes a bounded collaboration protocol that lets Claude Code
and Codex coordinate a single plan and then implement it through the shared
MCP server.

- **v1 (planning)**: bounded parallel drafts → canonical synthesis → Codex
  review → Claude finalize → `PlanLocked`. Two review rounds.
- **v3 (coding)**: post-`PlanLocked` task list → **batch implementation
  phase** (Claude orchestrates per-task subagents on its side via
  `superpowers:writing-plans` → `superpowers:subagent-driven-development`,
  then signals completion with `implementation_done`) → global 3-phase
  linear flow (Claude local → Codex review+fix → Claude final with PR URL)
  → `CodingComplete` / `CodingFailed`. Codex only participates at the
  global review stage; per-task implementation is single-agent on
  Claude's side.

This document covers:

- the full state machine and invariants (v1 + v3)
- the `collab_*` MCP tools
- topic payload formats for every protocol message
- harness-side responsibilities (git, cargo, gh, coderabbit)
- the autonomous long-poll loop each agent runs
- Claude's Plan Mode integration for canonical synthesis and revisions
- copy-pasteable prompts for the Claude and Codex terminals
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
| `canonical_plan` | Latest `canonical` message content (present when `canonical_plan_hash` is set). Lets a fresh agent rejoining mid-planning pull back its own earlier synthesis without a counterpart `recv`. |
| `final_plan_hash` | SHA-256 of the locked plan |
| `final_plan` | Latest `final` message content as sent — the JSON string `{"plan":"<full text>"}` (present when `final_plan_hash` is set). Primary input to the v2 `task_list` bridge after `PlanLocked`. |
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

After `task_list` lands, the session sits in a single Claude-owned phase
for the entire implementation run. Claude orchestrates per-task work
through `superpowers:writing-plans` (which produces a markdown plan at
`docs/superpowers/plans/...`) and then `superpowers:subagent-driven-development`
(which dispatches a fresh subagent per task, runs TDD, and commits per
task). The server stores the `task_list` manifest as an audit artifact
but does not iterate it; per-task progress is observable through the
git log on the branch.

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeImplementPending` | `claude` | `ImplementationDone{head_sha}` — fired once after the full subagent batch completes (gates green, all commits pushed) | `CodeReviewLocalPending` |

The `implementation_done` payload carries **only** `head_sha`. There is
no `notes`, `summary`, `subagent_report`, or any other field — Codex
reads the diff and the writing-plans markdown in the repo at the global
review stage and forms its own judgment.

### Global review, 3-phase linear

After `implementation_done`, the session enters a 3-turn linear review at
branch scope. Claude opens the PR on the final turn.

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeReviewLocalPending` | `claude` | `ReviewLocal{head_sha}` | `CodeReviewFixGlobalPending` |
| `CodeReviewFixGlobalPending` | `codex` | `CodeReviewFixGlobal{head_sha}` — Codex reviewed the full branch and (if needed) pushed fixes directly | `CodeReviewFinalPending` |
| `CodeReviewFinalPending` | `claude` | `FinalReview{head_sha, pr_url}` — Claude opens the PR and sends the URL in the same event | `CodingComplete` (terminal) |

### Shortcut: post-subagent coding review

When an orchestrator already completed the branch's implementation outside
Collab (including the local `/ultrareview-local` pass), it can skip v1
planning and the v3 batch implementation phase by calling
`collab_start_code_review`. The session starts directly at
`CodeReviewFixGlobalPending` with `current_owner = codex`.

The no-op handshake turn is collapsed: the ordinary full-flow global-review
path starts with Claude `ReviewLocal{head_sha}`, but the shortcut already
receives the branch `head_sha` at session creation time. From there, the
surviving flow is unchanged:

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeReviewFixGlobalPending` | `codex` | `CodeReviewFixGlobal{head_sha}` | `CodeReviewFinalPending` |
| `CodeReviewFinalPending` | `claude` | `FinalReview{head_sha, pr_url}` | `CodingComplete` |

Invariants that still apply:

- `collab_end` is rejected during both review phases, same as any other
  coding-active phase.
- `failure_report` is the only escape hatch and transitions to
  `CodingFailed`.
- Drift detection is special-cased for shortcut-started sessions:
  the server validates `CodeReviewFixGlobal{head_sha}` with a git
  ancestry check only when `task_list` is still unset. Full-flow v2
  sessions keep their existing non-shell-out behavior.

### Failure + terminal

| Phase | Owner | Event | Next |
|---|---|---|---|
| *any coding-active phase* | either | `FailureReport{coding_failure}` | `CodingFailed` (terminal) |

`collab_end` is **rejected** in every coding-active phase
(`CodeImplementPending`, `CodeReviewLocalPending`,
`CodeReviewFixGlobalPending`, `CodeReviewFinalPending`). Only
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
  "task": "design the marketing landing page"
}
```

Returns `{ session_id, task }`. The `task` is stored on the session so the
counterpart agent can read it via `collab_status` without a manual
paste.

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
through `CodeReviewFinalPending`). This prevents either agent from killing
a session the counterpart is still working in.

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
| `task_list` | `claude` | `{"plan_hash","base_sha","head_sha","plan_file_path"?,"tasks":[{"id","title","acceptance":[...]}]}` | `plan_hash` must equal `final_plan_hash`; `tasks` must be non-empty and strictly ordered by `id`; each task requires ≥1 `acceptance` entry. Optional `plan_file_path` (repo-relative; no leading `/`; no `..` segments) points at the writing-plans markdown driving subagent execution. |
| `implementation_done` | `claude` | `{"head_sha"}` | In `CodeImplementPending` only. Fired once after the subagent batch completes and gates pass. Carries only `head_sha` — no prose, no subagent notes. |
| `review_local` | `claude` | `{"head_sha"}` | Post-ultrareview, before handing to Codex for the global pass. |
| `review_fix_global` | `codex` | `{"head_sha"}` | In `CodeReviewFixGlobalPending` only. Codex has pushed any branch-level fixes. |
| `final_review` | `claude` | `{"head_sha","pr_url"}` | In `CodeReviewFinalPending` only. Claude has opened the PR; the event carries the URL and advances directly to `CodingComplete`. `pr_url` must start with `https://` and be ≤2048 chars. |
| `failure_report` | either | `{"coding_failure":"<reason>"}` | Valid in any coding-active phase. |

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
| `CodeImplementPending` | `implementation_done`, `failure_report` | v3 — single Claude turn after subagent batch |
| `CodeReviewLocalPending` | `review_local`, `failure_report` | |
| `CodeReviewFixGlobalPending` | `review_fix_global`, `failure_report` | |
| `CodeReviewFinalPending` | `final_review`, `failure_report` | |
| `CodingComplete` / `CodingFailed` | *(none — terminal; only `collab_end` accepted)* | |

`failure_report` is accepted from either agent in any coding-active phase
and transitions the session to `CodingFailed`. All other topics are gated
by the owner recorded in the phase table above.

## Harness-Side Responsibilities

The server validates transitions, persists hashes, and routes messages.
Most shell-level action — cargo, gh, coderabbit — is the **agent harness's**
responsibility. The protocol relies on the harness doing these things
between `wait_my_turn` and `collab_send`:

- **`base_sha` / `head_sha` tracking.** The harness records `base_sha` at
  `task_list` send time (the commit the branch forked from) and the current
  `head_sha` on every subsequent send. Before acting on an incoming turn,
  the harness reads `last_head_sha` from `collab_status` and runs
  `git cat-file -e <sha>^{commit}` to verify the commit is present; if not,
  it sends `failure_report` with `coding_failure: "branch_drift: ..."`.
- **Subagent orchestration** during `CodeImplementPending`. Claude invokes
  `superpowers:writing-plans` to expand the locked plan into a markdown
  task document, then `superpowers:subagent-driven-development` to
  dispatch fresh subagents per task. Each subagent runs TDD and commits
  on the branch. Per-subagent failures pause for triage; an unrecoverable
  failure surfaces as `failure_report` with `coding_failure: "subagent_failure: ..."`.
- **Local gates** before every Claude-owned coding turn
  (`implementation_done`, `review_local`, `final_review`):
  `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`.
  Any failure surfaces as `failure_report`; don't hide it.
- **Review + fix tooling** during Codex's `review_fix_global`:
  `coderabbit` / `/ultrareview-local` / manual review, followed by direct
  code edits + commit + push. Codex's judgment is expressed as commits,
  not prose.
- **Shortcut ancestry validation** during shortcut-started
  `review_fix_global`: the server shells out narrowly to `git
  merge-base --is-ancestor` to distinguish a true descendant check from
  operational git failures, and only applies that validation when
  `task_list` is still unset.
- **PR creation** during `final_review`: Claude runs `gh pr create
  --base <base_sha> ...` and sends the URL inline with the `final_review`
  event. There is no separate `pr_opened` turn.
- **Plan Mode** on Claude's side is entered before `canonical`, `final`
  (v1), and `final_review` (v3 PR creation). The `task_list` send is
  gated by writing-plans's own approval handoff (the user reviews the
  generated markdown and approves) rather than the harness's Plan Mode.
  Codex never enters Plan Mode.

The server does not read the git tree for the full v2 flow, and it still
trusts the harness's `head_sha` string there. The narrow shortcut-only
ancestry check is the exception; drift detection in that path is now a
hybrid responsibility, with the server performing the git ancestor check
and the harness still responsible for local verification and any
`failure_report` it emits.

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
| `PlanLocked` | exit loop (or send `task_list` to start v3) | exit loop |

Phase → action (v3):

| Phase | Claude does | Codex does |
|---|---|---|
| `PlanLocked` (post-final) | run `superpowers:writing-plans` on the locked plan; user approves the generated markdown; build `task_list` JSON (with `plan_file_path`), send | wait |
| `CodeImplementPending` | run `superpowers:subagent-driven-development` to dispatch per-task subagents; on full success run gates and send `implementation_done{head_sha}` | wait |
| `CodeReviewLocalPending` | run `/ultrareview-local`, fix HIGH/CRITICAL in place, send `review_local` | wait |
| `CodeReviewFixGlobalPending` | wait | review full diff, fix branch-level issues in place, send `review_fix_global` |
| `CodeReviewFinalPending` | gates, enter Plan Mode for PR title/body, `gh pr create`, send `final_review{pr_url}` | wait |
| `CodingComplete` / `CodingFailed` | exit loop | exit loop |

### Claude's Plan Mode Integration

Claude enters harness Plan Mode **only** before sending v1 `final` and v3
`final_review`. Everything before that — the initial draft, the canonical
synthesis, any revision rounds, and the v3 `task_list` send — runs
autonomously, with the user gating only at writing-plans's own approval
handoff during the v3 bridge.

The user is gated at v1 `final` because that's the planning commit point
(post-send the session is `PlanLocked`), and at v3 `final_review` because
that's where the PR is opened. Codex does not use Plan Mode — it posts
drafts, reviews, and global fixes directly.

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

## Codex handoff via MCP

Codex CLI sessions are one-shot: after sending a review and seeing the
session hand off back to Claude, Codex emits a summary and stops. It has
no `ScheduleWakeup` primitive to self-wake on the next handoff. Rather
than relying on an external daemon, Claude drives Codex's turn inline
via Codex's MCP server (`codex mcp-server`):

1. Register `codex mcp-server` with Claude Code (once):
   ```bash
   claude mcp add codex codex mcp-server
   ```
2. Claude's `/collab` prompt drives Codex whenever
   `current_owner == "codex"` (after a Claude send, on `/collab join`
   mid-session, or in the dispatch loop). **`codex mcp-server` does not
   resolve slash commands from `.codex-plugin/prompts/`.** Passing a
   raw `/collab join <sid>` string makes Codex treat it as ordinary
   user text and go off-script. Claude must expand the prompt locally:
   read `.codex-plugin/prompts/collab.md`, substitute `$ARGUMENTS` with
   `join <session_id>`, and call:
   ```json
   {
     "name": "mcp__codex__codex",
     "arguments": {
       "prompt": "<resolved prompt text from collab.md>",
       "cwd": "<repo_path>"
     }
   }
   ```
   The call blocks until Codex finishes its phase-specific action and
   hands control back. Claude then resumes the dispatch loop.

This keeps the control loop inside Claude Code — no external daemon, no
FIFO, no turn-change webhook. If the `codex` MCP server isn't registered,
the prompt falls back to asking the user to run `/collab join` manually.

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

Scope (v1 + v3):

- bounded planning (v1) and bounded coding loop (v3) through a single session
- one plan → one task list → one PR per session
- v1 planning is 2 review rounds; v3 coding is strictly linear (no rounds)
- Claude always gets the last word in planning (v1) and at the global
  review stage (v3)
- long-poll via `wait_my_turn`; agents run autonomously

Out of scope:

- multi-session orchestration
- parallel branches / concurrent PRs
- autonomous merge (Claude opens the PR; a human merges)
