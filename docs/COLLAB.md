# IronRace Collab (v1 Planning + v3 Coding)

`ironmem` includes a bounded collaboration protocol that lets Claude Code
and Codex coordinate a single plan and then implement it through the shared
MCP server.

- **v1 (planning)**: bounded parallel drafts → canonical synthesis → one
  copilot review pass → the pilot finalizes the approved task plan →
  `PlanLocked`.
- **v3 (coding)**: post-`PlanLocked` task list → **batch implementation
  phase** (the pilot publishes `task_list` from the approved task
  markdown, then the
  session's `implementer` runs per-task subagents via
  `iron-build` and signals completion with
  `implementation_done`) → global 3-phase linear flow (copilot review plus
  parallel fix fan-out → the pilot's local audit plus parallel fix fan-out →
  the pilot finalizes with PR URL) → `CodingComplete` / `CodingFailed`. Per-task
  implementation is single-agent on the selected implementer's side; the
  copilot always owns the first branch-scope review pass — Codex under the
  default `pilot=claude`, Claude under `pilot=codex`.

This document covers:

- the full state machine and invariants (v1 + v3)
- the `collab_*` MCP tools
- topic payload formats for every protocol message
- harness-side responsibilities (git, cargo, gh, pr-review-toolkit)
- Claude's dispatcher loop and one-shot Codex dispatches
- the dispatcher's Plan Mode integration for the locked plan at `PlanLocked` (the only planning user gate; the `final` send, the v3 bridge's parse, and `final_review` all run autonomously)
- copy-pasteable prompts (single-terminal default; Codex-terminal fallback)
- a worked example

The command surfaces and protocol prompts that agents actually run are derived
from this spec — keep them in sync when protocol changes land:

- `.claude-plugin/commands/collab.md` — Claude's `/collab` prompt.
- `.codex-plugin/commands/collab.md` — Codex's `/collab` slash command.
- `.codex-plugin/prompts/collab-plan-draft.md` — Codex's v1 draft turn.
- `.codex-plugin/prompts/collab-plan-synthesis.md` — Codex's v1 canonical-plan synthesis turn (pilot).
- `.codex-plugin/prompts/collab-plan-review.md` — Codex's v1 plan-review turn.
- `.codex-plugin/prompts/collab-plan-finalize.md` — Codex's v1 plan-finalize turn (pilot).
- `.codex-plugin/prompts/collab-task-list.md` — **installed but deliberately
  unrouted.** The file ships (the packaging test's `REQUIRED_CODEX_PHASE_PROMPTS`
  and this repo's lint both require it on disk), but the Codex shim's
  phase→prompt table carries **no `PlanLocked` row** and must never grow one.
  The live `PlanLocked` bridge is always executed by the Claude-side
  dispatcher's mechanical worker (`collab-turn-task-list.md`) under **either**
  pilot: the dispatcher-owned planning approval gate has to fire before any
  `task_list` send, and a one-shot `codex exec` cannot prompt a human. Routing
  `PlanLocked` through the shim would bypass that gate entirely.
- `.codex-plugin/prompts/collab-global-review.md` — Codex's v3 global-review/fix turn.
- `.codex-plugin/prompts/collab-review-local.md` — Codex's v3 post-fix local audit turn (pilot).
- `.codex-plugin/prompts/collab-final-review.md` — Codex's v3 final-review PR-body compose turn (pilot).
- `.codex-plugin/prompts/collab-recovery.md` — delegated v3 local/final-review recovery.
- `.codex-plugin/prompts/collab-batch-impl.md` — Codex's v3 batch-implementation turn.

## Collab issue task budget

One collab session implements exactly one independently shippable issue with
**1–10 execution tasks**. A plan projected to require 11 or more tasks is too
large for collab: split its scope into linked, independently executable child
issues before starting implementation. Route every child through
`/evaluate-issue`; only a child that itself receives a `COLLAB` verdict gets
its own 1–10-task collab session.

Do not make an oversized issue fit by merging unrelated work, weakening
acceptance criteria, or silently dropping tasks. During `/evaluate-issue`,
return the required `SPLIT` verdict and child-issue proposal; create the child
issues only after the user confirms. Keep the parent open as the tracking
issue. The MCP server rejects every `task_list` with more than 10 tasks as a
final invariant, even if an upstream prompt or evaluator misses the split.

## What It Is

IronRace Collab v1 is a **bounded planning protocol**, not an open-ended
multi-agent framework. Exactly one plan is produced per session, with:

1. two independent first drafts (Claude + Codex, blind to each other)
2. one canonical synthesis by the pilot
3. one review pass by the copilot
4. one final approved task plan sent autonomously by the pilot
5. terminal state `PlanLocked`, where the dispatcher — always Claude, see
   § Runtime Model → Roles — takes the single human planning gate before
   dispatching the `task_list` bridge, regardless of pilot

There is no `PlanEscalated` state and no re-synthesis loop. After the
copilot's one review pass, the pilot finalizes regardless of that verdict.

### Review cap (server-enforced)

`MAX_REVIEW_ROUNDS = 1` is the hard cap on copilot plan reviews, enforced
server-side at `crates/ironmem/src/collab/state_machine/mod.rs:28`
(the `PlanCodexReviewPending` transition always advances to finalization).
One review pass is a maximum, not an iteration target.

- After the copilot's `review` message, the server transitions to
  `PlanClaudeFinalizePending` **regardless of verdict** — `approve`,
  `approve_with_minor_edits`, and `request_changes` all map to the same
  next phase.
- The pilot has the last word on content: any unresolved review notes are
  absorbed (or explicitly declined with a rationale) in the `final` plan —
  composed and sent autonomously by whichever agent is piloting. The
  dispatcher's word comes after that, one phase later: it takes the single
  human planning gate at `PlanLocked`, on top of whatever the pilot composed
  and already sent.
- `review_round` is the audit trail. It is set to 1 after the copilot's
  review; post-finalize tests assert `review_round == MAX_REVIEW_ROUNDS`.
- The protocol is bounded by construction: at most one copilot review,
  then the pilot finalizes. Docs/prompts that frame v1 planning as
  open-ended iteration to convergence are wrong.

## Harness generalization vs two-party protocol

Issue #155 generalized ironmem's harness support into an extensible registry
while deliberately keeping `/collab` itself as a two-party Claude↔Codex
protocol. The two halves serve different concerns.

### Extensible via the harness registry

Generic AI assistant identity is represented by `HarnessId` and the
`crate::harness::REGISTRY` constant (`crates/ironmem/src/harness/mod.rs`).
The following generic surfaces consume registry metadata. Some outer
integration edges still have per-harness strategy code where the host harnesses
use different config files or plugin layouts.

- **Launcher metadata** — the existing `ironmem claude` / `ironmem codex`
  launcher commands derive their binary and label from `HarnessSpec`;
  `ironmem harnesses` dumps the full registry. Adding a registry entry does not
  create a new launch subcommand.
- **Attribution** — `classify_client_info` maps `clientInfo.name` to a
  harness id; `canonicalize_input` maps `IRONMEM_HARNESS` to a harness id.
- **Hook capabilities** — per-harness flags (`additional_context_support`,
  `occupancy_support`, `transcript_parser`) control what hook data ironmem
  captures.
- **Write-rules fanout** — `default_rules_targets` and the `--harness` flag now
  resolve strategy-bearing targets from each `HarnessSpec::rules_strategy` +
  `rules_file` pair, deduplicating only when both filename and strategy match.
  Native is canonical (`AGENTS.md`), while non-native strategies (Import/Copy)
  depend on canonical rules.
- **Doctor checks** — the `ironmem doctor` pass iterates the registry and emits
  a `harness_<id>` check per entry. Claude and Codex keep their current
  per-config detection strategies; additional entries report an advisory check
  until a detection strategy is added.
- **Metrics harness CHECK** — migration 013 relaxed the DB constraint from
  the hard-coded `'claude'/'codex'` domain to the `HarnessId` slug GLOB
  (`harness GLOB '[a-z0-9]*' AND harness NOT GLOB '*[^a-z0-9_-]*'`), so any
  registered harness can persist `token_usage`, `occupancy_samples`, and
  `session_summary` rows.
- **Packaging drift-lint** — `check_packaging_coverage` asserts that every
  registered harness has a corresponding `.<id>-plugin/` root with required
  wrapper assets.

Adding a new harness (e.g. Gemini) starts with one `HarnessSpec` in `REGISTRY`;
the generic surfaces above pick up the shared metadata from there. The harness
still needs its plugin root/assets, and any optional per-harness integration
strategy such as launcher or doctor detection must be added explicitly.

The write-rules registry contract is now strategy-aware:

- `rules_file` + `rules_strategy` are resolved together during write planning.
- `native` is only legal for `AGENTS.md`; non-native strategies reject
  `AGENTS.md`.
- Shared `rules_file` entries are allowed only when all strategies are
  identical.

### Intentionally still two-party: the `/collab` protocol

`/collab` is a **bounded two-party protocol** between Claude and Codex. This
is a deliberate design choice — the protocol's correctness proofs, state-machine
transitions, and load-bearing invariants all assume exactly two named parties.
The following are intentionally NOT generalized:

- **State machine** — the entire v1 + v3 state machine (`collab/mod.rs`,
  `collab/state_machine/`) names Claude and Codex as the two roles; turn
  ownership and the review-cap logic depend on this.
- **Blind dual-draft authoring** — the `PlanParallelDrafts` phase assumes
  exactly two independent drafters; `collab_recv` filters drafts by
  counterpart identity.
- **Single copilot counterpart-review pass** — the `MAX_REVIEW_ROUNDS = 1`
  cap is tied to one named counterpart; there is no general "other party"
  abstraction.
- **`collab_counterpart` role-flip** — the helper in
  `mcp/tools/shared.rs` maps `Claude → Codex` and `Codex → Claude`; it is
  exhaustive and closed.
- **`collab::Agent` role enum** — the closed two-variant enum
  (`Agent::Claude` / `Agent::Codex` in `collab/agent.rs`) is the
  compiler-enforced type for protocol roles. Exhaustiveness is load-bearing:
  `match` arms in the state machine must cover every variant, and the compiler
  enforces this. Adding a third variant would silently require updates across
  all match sites — the intentional design forces that cost to be visible.
  See the boundary note at the top of `collab/agent.rs`.
- **Pilot-configurability is not a step toward an N-party protocol.** The
  per-session `pilot` field (§ Runtime Model → Roles) lets either named agent
  lead planning and the v3 audit turns, but this is reuse of the two-party
  primitives above, not a loosening of them: `Agent` stays the closed
  two-variant enum, and `copilot()` is `counterpart(pilot)` — the same
  exhaustive `collab_counterpart` role-flip, just evaluated against whichever
  agent is currently piloting. `counterpart(x)` is only definable when there
  are exactly two participants (it returns "the other one"); it has no
  meaning for three or more. Making the pilot configurable does not open
  `Agent` to a third variant or generalize `counterpart` to an N-party
  lookup — a third protocol participant still requires the v2 redesign
  described above, independent of how many pilot values a two-party session
  supports.
- **`collab_sessions.implementer` CHECK** (migration 006) — the DB constraint
  pins the allowed implementer values to `'claude'` and `'codex'`.
- **`collab_actor_generations.agent` CHECK** (migration 010) — the
  generation-lease table's `agent` column is pinned to `'claude'` and
  `'codex'`.

Migrations 006 and 010 are explicitly left untouched by migration 013, which
notes: *"protocol-specific and are NOT touched here — they stay claude/codex by
design."*

### Adding a new harness does not make it a `/collab` participant

If a third harness is registered in `REGISTRY`, it immediately gains launcher,
attribution, hooks, write-rules, doctor, and metrics support. It does NOT
become a `/collab` participant. Extending the collab protocol to a third party
(or swapping Codex for a different counterpart) is a future protocol revision —
a v2 that redesigns the state machine, the DB schema constraints, and the
`Agent` enum — not a registry registration.

See the boundary doc comments at the top of
`crates/ironmem/src/collab/agent.rs` and
`crates/ironmem/src/collab/mod.rs` for the authoritative in-code statement of
this boundary.

### Codex-terminal-led sessions are a non-goal

The Codex-terminal `/collab join` path (§ Prompt Templates → "Joining a
session in a Codex terminal — fallback only") stays a one-shot shim by
design, not an unfinished feature. The Codex CLI cannot sustain a
`wait_my_turn` polling loop across handoffs, so it cannot host the
long-running dispatcher that `/collab`'s control loop requires (§ Runtime
Model → Roles: the dispatcher is always Claude for exactly this reason).
Generalizing the Codex-side prompt into a symmetric long-running dispatcher
is intentionally out of scope, for the same one-shot-CLI reason the
Codex-terminal path is a fallback at all.

The apparent requirement this would serve — "let Codex lead the session" —
is already met without it: `pilot=codex` in a Claude terminal makes Codex own
`canonical`, `final`, `task_list`, `review_local`, and `final_review`, so
every lead-role transition requires Codex as the actor. The Claude terminal
keeps running the dispatcher loop and talking to the human; Codex leads
planning and review as pilot. `dispatcher=claude, pilot=codex` delivers
Codex-led planning and review without needing the Codex CLI to poll.

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

### Roles

Four names recur throughout this document. Three are defined here; the
fourth, `implementer`, is defined in § Session State below.

- **dispatcher** — runs the control loop shown above and is the only role
  that talks to the human. It is determined by which terminal the session
  runs in, never persisted as a protocol role (there is no `dispatcher`
  column), and is always Claude in this codebase: the Codex CLI is one-shot
  and cannot sustain a `wait_my_turn` loop across handoffs, so only a Claude
  terminal can host the long-running dispatcher.
- **pilot** — the `collab_sessions.pilot` field, per-session, default
  `claude`. The pilot owns `canonical`, `final`, `task_list`,
  `review_local`, and `final_review`.
- **copilot** — `counterpart(pilot)`, derived on the fly and never stored.
  The copilot owns `review` and `review_fix_global`.

Consequence: **pilot is not the same role as dispatcher.** The dispatcher is
always Claude; the pilot can be either agent. "Codex-led role reversal in a
Claude terminal" is exactly `pilot=codex, dispatcher=claude` — the Claude
terminal keeps running the control loop and talking to the human, while
Codex leads planning and review as pilot. See also `implementer` (Session
State, below), a fourth, orthogonal knob that decides who writes the code.

## Session State

Stored in `collab_sessions`:

| Field | Meaning |
|---|---|
| `id` | Session identifier (returned from `collab_start`) |
| `repo_path`, `branch` | Where this plan applies |
| `task` | Human description of the planning goal. Set at `start`, readable via `status`. |
| `pilot` | Which agent leads v1 planning and the v3 review-audit turns (`claude` or `codex`); see § Runtime Model → Roles. Settable at `collab_start` and `collab_start_code_review`. Rebindable via `collab_set_pilot`, but only in `PlanParallelDrafts` before either first draft lands, and only by the agent that is currently the pilot — a copilot can never promote itself. Default `claude`. DB CHECK-constrained to `{claude, codex}` (migration 019). Orthogonal to `implementer` at validation time: the two columns are not validated against each other, and every combination (e.g. `pilot=codex` with `implementer=claude`) is legal — but an omitted `implementer` inherits `pilot` at defaulting time (see `implementer`, below). |
| `implementer` | Which agent runs the v3 batch implementation phase (`claude` or `codex`). Set at `start` and rebindable with `collab_set_implementer` while planning or `CodeImplementPending` is active. Defaults to the resolved `pilot` (so `pilot=codex` with no explicit `implementer` yields `implementer=codex`); pass `implementer` explicitly to split them. |
| `phase` | Current protocol phase (see below) |
| `current_owner` | Agent whose turn it is (`claude` or `codex`) |
| `claude_draft_hash`, `codex_draft_hash` | SHA-256 of each first draft |
| `canonical_plan_hash` | SHA-256 of the pilot's synthesis |
| `canonical_plan_ref` | The latest `canonical` plan reference (present when `canonical_plan_hash` is set): `{drawer_id, hash, plan_file_path}`. `plan_file_path` is null unless the plan carries the required leading marker. Status never includes the plan body; dereference the drawer only when required. See "Plan-by-reference contract". |
| `canonical_plan_drawer_id` / `final_plan_drawer_id` | Deterministic 32-char id of the `collab-plans` drawer storing the canonical/final plan body once accepted (migration 009). NULL on pre-009 sessions; status still exposes a body-free legacy reference when possible. |
| `final_plan_hash` | SHA-256 of the locked plan |
| `final_plan_ref` | The locked `final` plan reference (present when `final_plan_hash` is set): `{drawer_id, hash, plan_file_path}`. It is the primary input to the v3 `task_list` bridge after `PlanLocked`; the worker verifies the file against `hash`. Status never returns the normalized plan text. See "Plan-by-reference contract". |
| `task_list` / `task_list_ref` | The accepted v3 task-list reference: `{drawer_id, hash}` plus top-level `tasks_count`, `plan_file_path`, and `execution_mode`. `include_task_list:true` repeats this compact ref under `task_list`; it never inlines JSON. New sessions store the task list in `collab-task-lists`; pre-014 sessions may have `task_list_ref.drawer_id = null`. |
| `task_list_drawer_id` | Deterministic 32-char id of the `collab-task-lists` drawer storing the canonicalized task-list JSON once accepted (migration 014). NULL on pre-014 sessions. |
| `codex_review_verdict` | The **copilot's** verdict from the one review pass — Codex under the default `pilot=claude`, Claude under `pilot=codex`. The column name is historical and was deliberately not migrated. |
| `review_round` | Number of completed copilot reviews (0 or 1; planning has one review pass) |
| `ended_at` | Non-null once `collab_end` has been called |

All state changes are recorded in `wal_log`.

### Migration note: `pilot` column (schema v19)

The `pilot` column on `collab_sessions` was added by migration
`crates/ironmem/migrations/019_collab_pilot.sql`, bringing the schema to
version 19. It is declared `NOT NULL DEFAULT 'claude'` with
`CHECK (pilot IN ('claude', 'codex'))`, so **every pre-019 session reads
back as `pilot = "claude"`** with no data migration needed — the default
reproduces the hard-wired Claude-led behavior that predates this column,
and existing `/collab start` callers that omit `--pilot` keep their
original behavior unchanged.

`crates/ironmem/src/collab/handoff.rs:278-284` documents the dependency
this creates: `create_session` now writes `pilot` on every insert, so the
column must exist before `create_session` can succeed. That comment (on
the test fixture's `open()` helper) also notes that migration 019's
`ALTER TABLE` depends only on `collab_sessions` (created in migration
003), so it is safe to apply out of numeric sequence relative to
migrations 011-018 — which is exactly what the fixture does, stopping its
own replay at 010 for everything else while still applying 019 so
`create_session` succeeds.

No new environment variable is introduced by this change: `pilot` is set
exclusively via the `collab_start` / `collab_start_code_review` `pilot`
argument (defaulting to `"claude"` when omitted) or reassigned post-creation
via `collab_set_pilot` — there is no `IRONMEM_*` toggle for it.

## Phase Model

**Wire-compat note:** two `Phase` variants were renamed in
`crates/ironmem/src/collab/phase.rs` when the pilot/copilot role split was
generalized, but their serialized wire strings — the values actually stored
in `collab_sessions.phase` and used as headings/labels below — were kept
byte-identical for backward compatibility with in-flight sessions:
`Phase::PlanCopilotReviewPending` still serializes as `"PlanCodexReviewPending"`,
and `Phase::PlanFinalizePending` still serializes as
`"PlanClaudeFinalizePending"`. Read every `PlanCodexReviewPending` /
`PlanClaudeFinalizePending` heading below as the frozen wire name, not a
claim about which agent owns the turn — ownership is `pilot`/`copilot`, per
§ Runtime Model → Roles. Likewise, the `claude_draft_hash` /
`codex_draft_hash` columns (§ Session State) are documented, not migrated:
they stay keyed by agent *identity* (`claude_draft_hash` always holds
Claude's draft, `codex_draft_hash` always holds Codex's), independent of and
orthogonal to whichever agent is piloting.

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
the pilot (`current_owner`).

### `PlanSynthesisPending`

Owner: the pilot (`current_owner`). The pilot sends one `canonical` message
containing the merged plan: `collab-turn-plan-synthesis.md` when
`pilot == "claude"`, or Claude's loop triggers Codex's equivalent turn
(`collab-plan-synthesis.md`, which sends `canonical` itself) when
`pilot == "codex"`.

This phase is autonomous. Neither agent enters Plan Mode and neither asks for
approval here; the single planning gate is the dispatcher's, at `PlanLocked`,
two phases later.

Exit → `PlanCodexReviewPending`, owner the copilot (`current_owner`).

### `PlanCodexReviewPending`

Owner: the **copilot** — `require_actor(actor, copilot(session))` in
`crates/ironmem/src/collab/state_machine/mod.rs`, where
`copilot(session) = counterpart(session.pilot)`. The phase label is the frozen
wire name, not an owner claim: under the default `pilot == "claude"` the
copilot is Codex, but under `pilot == "codex"` this is **Claude's** turn. Read
`current_owner` rather than assuming Codex. The copilot sends one `review`
with a verdict:

- `approve`
- `approve_with_minor_edits`
- `request_changes`

Exit:

- Always → `PlanClaudeFinalizePending`, owner the pilot (`current_owner`).

The copilot must put all requested edits, risks, and task-splitting concerns
into this one review pass. In particular, any task that looks larger than 20
minutes or any scope that credibly needs more than 10 tasks must be called out
so the pilot can split it into independently executable child issues before
finalization.

### `PlanClaudeFinalizePending`

Owner: the pilot (`current_owner`). **No human gate here — this turn is
autonomous.** The single human planning gate belongs to the dispatcher, and
it fires one phase later, at `PlanLocked`, before the bridge dispatches
`task_list` (see `.claude-plugin/commands/collab.md` § v3 Bridge, step 0) — a `codex exec` one-shot cannot prompt
a human, so only the dispatcher can own a human gate, and under
`pilot == "codex"` this finalize turn belongs to Codex regardless. Composition
and the send are pilot-generic: `collab-turn-plan-finalize.md` writes the
final iron-build-compatible task markdown when `pilot == "claude"`, or
Claude's loop triggers Codex's equivalent turn when `pilot == "codex"`, and
either way `collab-turn-submit.md` sends the one `final` message as the
pilot. Every task must be scoped to 20 minutes or less, and the plan must
contain 1–10 tasks. If it would need 11 or more, stop before sending `final`
— that send, not the later `PlanLocked` gate, is the point of no return:
once `PlanLocked` is reached the plan is immutable, and an oversized
`task_list` is rejected on the `> 10` task-count check with `collab_end` as
the only exit. Split larger work into independently executable child issues
before sending `final`, never at the gate.

Exit → `PlanLocked` (always). Planning is done.

### `PlanLocked`

Plan is frozen; `final_plan_hash` is set. This is terminal for `wait_my_turn`
**only while `task_list` has not yet been submitted**.

The dispatcher takes the single human planning gate here — after `final` is
already on the wire, before `task_list` is dispatched (see
`.claude-plugin/commands/collab.md` § v3 Bridge, step 0; § Approval gates are
reference-only, below). It surfaces ONLY
`{drawer_id, plan_file_path, ≤3-line summary}` for approval, never the plan
body. Two transitions out:

- `collab_end` — abandon before coding starts, on gate rejection (last point
  this is valid).
- `collab_send` with `topic=task_list` from the pilot (`current_owner`), on
  gate approval — enter the v3 coding loop. The state machine verifies
  `plan_hash == final_plan_hash` and that the task list contains 1–10 tasks;
  the session stays active and the terminal set for `wait_my_turn` flips to
  `{CodingComplete, CodingFailed}`.

## v3 Coding Phase Model

v3 reuses the same session (no new `id`). It extends `collab_sessions` with
a `base_sha` / `last_head_sha` pair for branch-drift detection, `pr_url`
for the PR handoff, and `coding_failure` for unrecoverable errors. Each
phase names the exact event that advances it.

v3 is deliberately linear: every turn deterministically advances to the
next phase. There are no debate rounds, no verdicts, no round counters
at the coding stage. This structurally prevents the orchestrator from
steering the reviewer's conclusion — the copilot's only coding turn is at the
global review stage and is expressed as commits, not prose.

### Batch implementation

After `task_list` lands, the session sits in a single phase for the
entire implementation run. Which agent owns that phase depends on the
session's current `implementer` field:

- **`implementer == "claude"`** (default): Claude orchestrates per-task
  work from the approved task markdown through
  `iron-build` (fresh subagent per task, TDD, per-task
  commits). Claude emits `implementation_done`.
- **`implementer == "codex"`** (opt-in via
  `/collab start --implementer=codex` or
  `/collab join --implementer=codex <session_id>`): the pilot still publishes
  `task_list` from the approved task markdown. Then Claude dispatches Codex via
  background `codex exec` (per Implementation Notes § Background
  `codex exec` dispatch; `mcp__codex__codex` is the fallback when
  `codex` is not on PATH). Codex runs its own
  `iron-build` (controller-owned loop, runs to
  completion) and emits `implementation_done` itself before the
  bg-exec settled wait wakes on the phase advance.

In both modes the server stores the `task_list` manifest as an audit
artifact but does not iterate it. Per-task progress is observable through
the git log on the branch and through durable ironmem checkpoints written
by the implementer after each task boundary. After `implementation_done`,
the phase advances to `CodeReviewFixGlobalPending` with the **copilot** as
owner regardless of who implemented — the copilot runs
`/pr-review-toolkit:review-pr` on the raw post-implementation diff first
(no pre-clean by the pilot) and
uses parallel fix subagents for confirmed, partitionable findings. The pilot's
`review_local` then audits the copilot's commits at `CodeReviewLocalPending`
(full `/ultrareview-local` unless reduced-mode criteria apply) and uses the
same fix fan-out model for confirmed audit findings.

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeImplementPending` | `claude` or `codex` (per session `implementer`) | `ImplementationDone{head_sha}` from the implementer agent — fired once after the full subagent batch completes (gates green, all commits pushed) | `CodeReviewFixGlobalPending` (copilot-owned) |

The `implementation_done` payload carries **only** `head_sha`. There is
no `notes`, `summary`, `subagent_report`, or any other field — the
non-implementer agent reads the diff and the approved task
markdown in the repo (via `plan_file_path`) at the global review stage and forms
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
- `logical_key`: `collab-checkpoint:<session_id>`
- One checkpoint before starting each task (`status: started`)
- One checkpoint after each task is implemented, reviewed, committed, and
  pushed (`status: completed`)
- One checkpoint on unrecoverable task failure (`status: blocked`)
- One final checkpoint before `implementation_done`
  (`status: batch_complete`)

Every checkpoint write for a session uses that same logical key, replacing the
one logical-keyed current drawer instead of appending another checkpoint.
Carry `completed_task_ids` forward in the replacement content so the drawer
always contains the full recovery state.

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
gates_sha: <HEAD sha that gates ran against|none>
gates_commands: <exact gate commands separated by " && "|none>
gates_result: <not_run|passed|failed: short reason>
summary: <one concise sentence>
resume_hint: /collab join [--implementer=<claude|codex>] <session_id>
```

On any fresh `/collab join` that lands in `CodeImplementPending`, the owning
implementer must fetch the one logical-keyed current drawer deterministically
with `get_drawer(wing=ironrace-memory, room=collab-checkpoints,
logical_key=collab-checkpoint:<session_id>)`, then use that checkpoint plus the
git log to choose the first unfinished task. The new implementer must then read
the plan and scan the current code/diff to
verify which acceptance criteria are already complete before editing. If
the current checkpoint is `batch_complete`, first try to reuse its gate
proof: require clean pushed-head proof, local `HEAD == checkpoint.head_sha`,
`checkpoint.gates_sha == checkpoint.head_sha`, `checkpoint.gates_result`
starts with `passed`, and `checkpoint.gates_commands` exactly matches the
current required gate set. When all checks hold, send
`implementation_done` without rerunning gates. Rerun gates only when HEAD
drifted, the gate command set changed, the pushed-head proof fails, or the
checkpoint lacks the new gate-proof fields. Otherwise resume at
`next_task_id` (or the `started` task if the last checkpoint stopped
mid-task).

**Both modes apply the same *Finishing the Branch* carve-out**: the
implementer agent stops `iron-build` at the last task's approval+commit and
does *not* let the skill run its *Finishing the Branch* step. `iron-build`
documents the matching halt contract on its side — "an orchestrator that owns
its own PR path (such as `/collab`) will treat a PR created here as a protocol
violation" — so the boundary is stated from both ends rather than assumed from
one. PR creation belongs to the collab `final_review` turn, not to the
sub-skill. The Claude implementer verifies this as a local boundary invariant
and does not query GitHub by default. A `gh pr list --head <branch>` probe is
opt-in only when the controller reports boundary uncertainty or the worker
output mentions PR creation or the *Finishing the Branch* step.

### Global review, 3-phase linear (the copilot first; the pilot audits after)

After `implementation_done`, the session enters a 3-turn linear review at
branch scope. The copilot runs `/pr-review-toolkit:review-pr` on the raw
post-implementation diff first, then fans confirmed findings out to
parallel fix subagents where they can be safely partitioned; the pilot then
runs the `review_local` audit of the copilot's commits and fans confirmed audit
findings out the same way; the pilot also owns the final protocol identity on
the final turn.

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeReviewFixGlobalPending` | `copilot` | `CodeReviewFixGlobal{head_sha}` — the copilot ran `/pr-review-toolkit:review-pr` on the full diff AS-IS (no pre-clean by the pilot), partitioned confirmed findings, used parallel fix subagents where safe, merged/cherry-picked the fixes, then pushed | `CodeReviewLocalPending` |
| `CodeReviewLocalPending` | `pilot` | `ReviewLocal{head_sha}` — the pilot ran full or reduced `review_local` audit of the copilot's commits + issues both agents missed, partitioned confirmed findings, used parallel fix subagents where safe, merged/cherry-picked the fixes, then pushed | `CodeReviewFinalPending` |
| `CodeReviewFinalPending` | `pilot` | `FinalReview{head_sha, pr_url}` — the pilot owns the event identity; in normal flow the Claude submit worker opens the PR and sends the URL under that identity | `CodingComplete` (terminal) |

The `Owner` column is the normal flow. Under the delegated-completion
override, the recovery owner sends the interrupted phase's event in the
original owner's place — including `final_review`, in which case the
recovery owner opens the PR instead. See "Failure + terminal" above and the
PR-ownership rule under "Harness-Side Responsibilities".

### Shortcut: post-iron-build coding review

When an orchestrator already completed the branch's implementation outside
Collab, it can skip v1 planning and the v3 batch implementation phase by
calling `collab_start_code_review`. The session starts directly at
`CodeReviewFixGlobalPending` with `current_owner` set to the copilot
(`counterpart(pilot)` — Codex under the default `pilot == "claude"`).

Because shortcut sessions have no collab `task_list`, the copilot must recover
the implementation context before reviewing: search ironmem checkpoints
for the same `repo_path`/`branch`, read any referenced approved task
markdown, and scan the current code/diff to determine which acceptance
criteria are already complete. If no checkpoint exists, fall back to the
branch diff plus nearby plan docs under `docs/iron/plans/`.

The no-op handshake turn is collapsed: `head_sha` is supplied at session
creation time. From there, the surviving flow follows the new ordering:
the copilot's `review_fix_global` (`/pr-review-toolkit:review-pr` plus parallel
fix fan-out for confirmed findings on the raw diff) → the pilot's
`review_local` (audit the copilot's commits plus parallel fix fan-out) → the
pilot's `final_review` (PR creation).

| Phase | Owner | Event | Next |
|---|---|---|---|
| `CodeReviewFixGlobalPending` | `copilot` | `CodeReviewFixGlobal{head_sha}` | `CodeReviewLocalPending` |
| `CodeReviewLocalPending` | `pilot` | `ReviewLocal{head_sha}` | `CodeReviewFinalPending` |
| `CodeReviewFinalPending` | `pilot` | `FinalReview{head_sha, pr_url}` | `CodingComplete` |

Invariants that still apply:

- `collab_end` is rejected during all review phases, same as any other
  coding-active phase.
- `failure_report` is the only escape hatch. A **Terminal**-classified
  report transitions to `CodingFailed`; a **Tooling**-classified report
  (six recoverable prefixes — see "Failure + terminal") instead keeps the
  session at its current phase and flips `current_owner`.
- Drift detection is special-cased for shortcut-started sessions:
  the server validates `CodeReviewFixGlobal{head_sha}` **and**
  `ReviewLocal{head_sha}` with a git ancestry check when `task_list` is
  still unset. Both the copilot's `review_fix_global` push and the pilot's
  `review_local` audit-push must descend from the prior `last_head_sha`.
  Full-flow v3 sessions keep their existing non-shell-out behavior.
- **Scoped attribution:** one live collab session may own each
  `(repo_path, branch)` scope. A shared daemon may therefore run live sessions
  for different repositories or branches concurrently. A second live session
  in the *same* scope is refused; use another branch, or finish the existing
  session first. Missing and ended bindings self-heal only in their own scope
  and never clear an unrelated live session.

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

`failure_report` failures classify into two severities. Classification is
server-authoritative (`classify` in
`crates/ironmem/src/collab/failure_class.rs`, re-exported from
`collab::mod`) — an agent never decides its own
severity, it only picks an accurate `coding_failure` prefix.

| Severity | Behavior |
|---|---|
| **Tooling** (recoverable) | Session **stays in its current phase**; `current_owner` flips to the counterpart agent, who must recover the turn. |
| **Terminal** (unrecoverable) | Session transitions to `CodingFailed`. |

#### The six recoverable prefixes

A `coding_failure` classifies **Tooling** only if it starts with one of
these prefixes AND has >=1 byte of detail after the colon. A bare prefix
with nothing after it — like every other unrecognized string — classifies
**Terminal**:

| Prefix | Example |
|---|---|
| `git_commit_failed:` | `git_commit_failed: index.lock EPERM` |
| `git_push_failed:` | `git_push_failed: remote rejected` |
| `sandbox_denied:` | `sandbox_denied: write outside workspace` |
| `disk_full:` | `disk_full: no space left on device` |
| `network_failed:` | `network_failed: connection reset` |
| `codex_dispatch_failed:` | `codex_dispatch_failed: process exited 137` |

Everything else classifies **Terminal**: a bare recoverable prefix with no
suffix, `branch_drift:` (see the drift check in "Harness-Side
Responsibilities" below), `subagent_failure:`, any unrecognized string, and
the empty string.

| Phase | Owner | Event | Next |
|---|---|---|---|
| *any coding-active phase* | owner; `codex_dispatch_failed:` may also be reported by Claude while Codex owns the interrupted turn | `FailureReport{coding_failure}` classifying **Tooling** | same phase; `current_owner` becomes the counterpart of the interrupted turn owner |
| *any coding-active phase* | owner; `branch_drift:` with detail may also be reported by either agent | `FailureReport{coding_failure}` classifying **Terminal** | `CodingFailed` (terminal) |

#### Two independent axes: off-turn admissibility vs recoverable classification

These are separate questions decided by separate code, and conflating them
produces wrong expectations:

- **Admissibility** — *may a non-owner send this report at all?* Decided by
  `off_turn_failure_is_admissible` against `OFF_TURN_FAILURE_PREFIXES`
  (`branch_drift:`, `codex_dispatch_failed:`).
- **Classification** — *does this report recover or kill the session?*
  Decided by `failure_class::classify` against
  `RECOVERABLE_FAILURE_PREFIXES` (the six prefixes above). The reporter's
  identity is irrelevant here.

The two vocabularies overlap but are not the same set.
`codex_dispatch_failed:` is in both — off-turn-admissible *and* Tooling.
`branch_drift:` is in only the first: **off-turn-admissible but always
Terminal**, because it is not in `RECOVERABLE_FAILURE_PREFIXES`. Either
agent may report drift at any time, and every such report ends the session
in `CodingFailed` — drift means the two agents disagree about what is on
the branch, which no in-place recovery turn can reconcile. The remaining
five recoverable prefixes are Tooling but owner-only.

`collab_end` is **rejected** in every coding-active phase
(`CodeImplementPending`, `CodeReviewFixGlobalPending`,
`CodeReviewLocalPending`, `CodeReviewFinalPending`). Only
`CodingComplete` or `CodingFailed` end the session post-`task_list`.

**There is no `collab_end` exit from an in-flight recovery.** A Tooling
report keeps the session in its coding-active phase indefinitely, and that
phase is exactly where `collab_end` is rejected — so an operator who wants
to abandon a session mid-recovery must first drive it to a terminal phase.
The intended procedure is one of:

- (a) let the recovery owner complete the interrupted turn (the session
  advances normally, and `collab_end` becomes valid at `CodingComplete`);
  or
- (b) drive the session to `CodingFailed` — either by exhausting a retry
  ceiling below, or by sending a Terminal-classified `failure_report`
  (e.g. `subagent_failure: operator abandoned mid-recovery`). `collab_end`
  is valid from `CodingFailed`.

Option (b) is the deliberate abandon path. It is not a workaround: the
`CodingFailed` row records `coding_failure` and `failed_from_phase`, so
the abandonment is auditable, and — if the recorded failure classifies
Tooling and the lifetime ceiling below is not exhausted — the session
stays eligible for `collab_resume` later.

#### Retry ceilings: `MAX_RECOVERY_ATTEMPTS = 2` and `MAX_TOTAL_RECOVERY_ATTEMPTS = 5`

Two counters bound recovery, and a Tooling `failure_report` degrades to the
terminal `CodingFailed` path when **either** would be exceeded — that is,
when `recovery_attempts + 1 > MAX_RECOVERY_ATTEMPTS` **or**
`total_recovery_attempts + 1 > MAX_TOTAL_RECOVERY_ATTEMPTS`. The degrade
uses that report's own diagnostic, not an earlier attempt's.

| Counter | Scope | Incremented | Reset |
|---|---|---|---|
| `recovery_attempts` (ceiling 2) | **per resume** — the budget for the current recovery streak | on every accepted Tooling recovery handoff | to `0` on a successful delegated completion, and on `ResumeCoding` (`collab_resume`) |
| `total_recovery_attempts` (ceiling 5) | **lifetime of the session** | on every accepted Tooling recovery handoff | **never** — not by a successful completion, not by `collab_resume`, not by anything |

`collab_resume` is additionally rejected with `NotResumable` once
`total_recovery_attempts >= MAX_TOTAL_RECOVERY_ATTEMPTS`, so an exhausted
session cannot be resurrected into another recovery streak.

**Why 5 and not a multiple of 2.** The lifetime ceiling is deliberately
*not* a multiple of `MAX_RECOVERY_ATTEMPTS`. In exactly the loop it exists
to stop — exhaust the budget, resume, exhaust it again, with no successful
completion in between — the lifetime count advances in lockstep with the
per-resume budget and therefore lands only on multiples of 2. A lifetime
ceiling of 4 or 6 would only ever be reached on a report the per-resume
ceiling already degrades, so it would never be the binding check on that
path: unreachable in the one scenario it was written for, and
indistinguishable from a missing check. At 5 it genuinely binds: two exhausted
budgets put the lifetime count at 4, a `collab_resume` refills the
per-resume budget to 0, the next accepted handoff takes the lifetime count
to 5, and the one after that degrades the session to `CodingFailed` on the
lifetime ceiling alone — with the per-resume budget still unspent at 1.

**Why the lifetime counter exists.** `collab_resume` is agent-callable and
sits in the unattended-successor permission allowlist, and it zeroes
`recovery_attempts`. Before `total_recovery_attempts` existed, the
per-resume budget was the only bound, so an autonomous agent could loop
forever: N tooling failures → ceiling → `CodingFailed` → `collab_resume` →
budget back to 0 → N more failures, burning tokens indefinitely. No
surface could show a count above `2`, because the only counter that existed
was the one being reset — `collab_status` and the handoff block both
reported a session mid-loop as healthy. `total_recovery_attempts` is
monotonic precisely so that the loop terminates and so that the true
recovery cost of a session is visible on both surfaces.

Both counters are exposed by `collab_status` and rendered in the
`session_handoff` block, alongside `recovery_origin_owner`.
`total_recovery_attempts` is a nullable `INTEGER` column on
`collab_sessions` added by migration 015; legacy rows store `NULL` and read
back as `0`.

#### Reporter and recovery-owner protocol (operator instructions — not server-enforced)

When an agent hits a recoverable tooling failure — e.g. Codex cannot `git
commit` from inside a linked worktree — it sends `failure_report` with a
recoverable prefix and a real detail suffix, e.g. `git_commit_failed:
index.lock EPERM`. The phase does not change; only `current_owner` flips
to the counterpart. Two rules apply to both agents by convention — the
server does not enforce either:

- **The reporter MUST leave the working tree and diff intact.** Do not
  `git reset --hard` or otherwise discard staged/unstaged work when
  reporting a tooling failure — the counterpart needs that exact diff to
  finish the turn. (This is distinct from the ordinary pre-send sync
  described in "Harness-Side Responsibilities" below, which resets to
  `last_head_sha` at the *start* of a turn — not while abandoning one
  mid-flight.)
- **The reporter MUST NOT retry the same failing operation in a loop**
  hoping it magically works. Report once per genuine attempt and let the
  counterpart recover.

For an ordinary owner-reported failure, the recovery owner is the reporter's
counterpart. `codex_dispatch_failed:` is off-turn-admissible only when Claude
reports it against a Codex-owned turn. (`branch_drift:` is off-turn-admissible
for either agent but classifies Terminal, so it never produces a recovery owner
— see "Two independent axes" above.) When Claude observes an unavailable Codex turn
and reports `codex_dispatch_failed:`, recovery stays with Claude (the
counterpart of the interrupted Codex owner), rather than being handed back to
the unavailable process. The recovery owner (the agent `current_owner` now
names) MUST:

1. Inspect the preserved diff/working-tree state the reporter left behind.
2. Run whatever gates apply to the interrupted phase itself — the protocol
   has no way to verify the reporter's incomplete work was safe.
3. Commit and push the result itself. This is the actual fix for the
   worktree-`git commit` scenario above: a different agent, with different
   sandbox/git permissions, finishes the operation the reporter couldn't.
4. Send the phase's **normal** completion event (`implementation_done`,
   `review_fix_global`, `review_local`, or `final_review`, whichever the
   interrupted phase expects). The delegated-completion mechanism accepts
   the recovery owner's normal completion event as if it were the original
   owner's.

   **Do not re-report the failure you were handed.** Echoing the same
   tooling failure back (`git_commit_failed:` again, because you did not
   actually try a different approach) does not complete the turn — it just
   burns a retry attempt against both ceilings and flips ownership back.
   Recovery means attempting the operation with your own tools and
   permissions, not forwarding the diagnostic.

   This is **not** a blanket ban on `failure_report` during recovery. A
   *genuinely new* failure hit while recovering still gets its own report:
   a real gate failure, an unrecoverable subagent failure, drift detected
   on the branch. Report it with the prefix that accurately describes what
   you hit — and accept that if it classifies **Terminal** (e.g.
   `subagent_failure:`, `branch_drift:`) it correctly ends the session in
   `CodingFailed` rather than recovering again. The agent prompts instruct
   exactly this; an accurate terminal report is the right outcome, not a
   protocol violation.

#### `pending_failure` vs `coding_failure`

- `pending_failure` holds the diagnostic for an **in-flight** recoverable
  (Tooling) failure. It is set while the session stays in its
  coding-active phase awaiting recovery, and is mutually exclusive with
  `coding_failure`.
- `coding_failure` is reserved for the **terminal** cause. It is only ever
  set when the session actually enters `CodingFailed` — either a genuine
  Terminal report, or a Tooling report that broke either retry ceiling.

#### `collab_resume`

`collab_resume` (a separate MCP tool — see below — not a `collab_send`
topic) lets either agent restore a `CodingFailed` session back to its
`failed_from_phase`. It is eligible only when ALL THREE hold:

- the stored `coding_failure` classifies **Tooling**,
- `failed_from_phase` was actually recorded, and
- `total_recovery_attempts < MAX_TOTAL_RECOVERY_ATTEMPTS` (5) — the
  monotonic lifetime budget is what makes resume terminate rather than
  refill itself forever.

A session that predates this feature has `failed_from_phase = NULL` and
`collab_resume` returns a deterministic `NotResumable` error naming that
the session "predates resume support" — never a guess about what actually
happened during coding. This path is for when the per-resume retry ceiling
was exceeded, or a genuinely fresh process wants to pick a
dead-but-recoverable session back up — it is not needed for the normal
in-flight recovery described above, since that session never leaves its
phase in the first place.

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
  "pilot": "claude",
  "implementer": "claude"
}
```

Returns `{ session_id, task, implementer, pilot }`. The `task` is stored on
the session so the counterpart agent can read it via `collab_status`
without a manual paste. `pilot` is optional, defaults to `"claude"`, and
must be one of `{"claude","codex"}` — it selects which agent leads v1
planning and the v3 review-audit turns (§ Runtime Model → Roles). The DB
CHECK constraint on the `pilot` column enforces the same set (migration
019), so direct writes cannot bypass validation. `implementer` is
optional and must be one of `{"claude","codex"}` — it routes the v3
`CodeImplementPending` phase to the named agent, defaulting to the
resolved `pilot` (so an omitted `implementer` is `"claude"` under the
default `pilot=claude`, or `"codex"` if `pilot=codex` was passed). The DB
CHECK constraint on the `implementer` column enforces the same set, so
direct writes cannot bypass validation.

**Duplicate-session guard.** `collab_start` (and `collab_start_code_review`)
reject the call when a session that still reserves the same `repo_path` +
`branch` exists. This prevents an accidental second session — most often a
fired `ScheduleWakeup` replaying the `/collab start` entry command. Different
repositories and branches may proceed concurrently through one shared daemon.
`CodingComplete` releases the start slot while it awaits `collab_end`, so a
new session may claim that scope; `CodingFailed` deliberately keeps its slot.

`collab_end` still matters. From `CodingComplete` it is the operator's
completion attestation; from `CodingFailed` it records the terminal end when
the session will not be retried. A tooling-class `CodingFailed` session that
has not been ended remains resumable and continues to own its scope.

**Scoped attribution guard.** `collab_start`, `collab_start_code_review`,
`collab_send`, `collab_recv`, `collab_wait_my_turn`, and `collab_resume` only
conflict with a different still-live session in the same `(repo_path, branch)`
scope. The refusal names the existing session and tells the caller to end it
or use a different repository branch. Stale, missing, and ended bindings
self-clear automatically only for their matching scope; no manual cleanup or
separate daemon is needed for independent repos or branches. On `"could not
verify active collab session"`, check the server
logs for the underlying DB error detail and retry after that issue clears.

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
one current logical-keyed ironmem checkpoint, then scans the plan and current code before
continuing. Calls after `implementation_done` are rejected.

### `collab_set_pilot`

Reassigns the session's `pilot` — far more restricted than
`collab_set_implementer` beside it. See § Session State and § Runtime
Model → Roles for the `pilot`/`copilot` vocabulary this section assumes.

```json
{
  "session_id": "...",
  "agent": "claude",
  "pilot": "codex"
}
```

All three fields are required: `session_id`, `agent` (the caller's claimed
identity), and `pilot` (the agent to reassign the role to) — the latter two
each one of `{"claude","codex"}`.

**Caller restriction.** Only the session's *current* pilot may call this,
checked before phase, so an unauthorized caller is rejected identically
regardless of what phase the session is in. A copilot can never promote
itself; the rejection names the caller's role and the pilot that may act:
`"collab_set_pilot refused: caller '<agent>' is the copilot of this
session; only the current pilot '<pilot>' may reassign the pilot role"`.
This is what stops the tool from being a turn-seizure primitive — the only
way to become pilot is to be handed the role by the agent that already
holds it.

**Phase policy.** Legal only in `PlanParallelDrafts`, and only before
either agent's first draft has landed. It is rejected:
- once a draft (`claude_draft_hash` or `codex_draft_hash`) has been
  submitted, even while still in `PlanParallelDrafts`;
- in any other phase, e.g. `CodeReviewFixGlobalPending`;
- on a `collab_start_code_review` session, which begins at
  `CodeReviewFixGlobalPending` and so never passes through the one phase
  where reassignment is legal — its pilot is fixed at creation.

**On success**, `current_owner` moves to the new pilot in the same
transactional update, so pilot and owner are never observable in an
inconsistent pairing.

**No-partial-write guarantee.** A refused call — whether for the caller
restriction or the phase policy — leaves both `pilot` and `current_owner`
unchanged; every check runs inside the request's transaction before any
write.

See `crates/ironmem/tests/mcp_protocol.rs:2760-2997` for the caller and
phase-policy tests, including the self-promotion refusal in both
pilot directions.

### `collab_start_code_review`

Shortcut entry. Creates a session positioned at `CodeReviewFixGlobalPending`,
owned by the **copilot** (`counterpart(pilot)`) — Codex under the default
`pilot=claude`, but Claude when `pilot=codex` is passed. See the "Shortcut:
post-iron-build coding review" subsection above for the constraints and
surviving flow.

```json
{
  "repo_path": "/path/to/repo",
  "branch": "feat/landing-page",
  "base_sha": "abc123",
  "head_sha": "def456",
  "initiator": "claude",
  "task": "add landing page",
  "pilot": "claude"
}
```

Returns `{ session_id, task, pilot }`. The `task` is stored on the session
and is readable via `collab_status`. `pilot` is optional, defaults to
`"claude"`, and must be one of `{"claude","codex"}` — same DB CHECK as
`collab_start` (migration 019). `initiator` stays fixed to `"claude"`
regardless of `pilot`: it names the dispatcher invoking the shortcut, not
the review-flow lead (§ Runtime Model → Roles).

### `collab_send`

Sends a protocol message and advances the state machine.

```json
{ "session_id": "...", "sender": "claude", "topic": "draft", "content": "..." }
```

v1 planning topics: `draft`, `canonical`, `review`, `final`.

v3 coding topics: `task_list`, `implementation_done`, `review_local`,
`review_fix_global`, `final_review`, `failure_report`.

Non-advancing topic: `orphan_recovered` — recorded, never applied as an event.

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
| `full` | boolean | `false` | Compatibility-only: when `true`, additionally includes each current message's inline `content` alongside its drawer reference. Prefer `get_drawer` with the returned `drawer_id`. |

Using `auto_ack=true` is recommended in the dispatch loop for any caller that
always acks all received messages immediately. The explicit `collab_ack` call
is still available when callers need selective acknowledgement.

#### Compact message-reference contract

The default response is `{ "messages": [...] }`. Each current message keeps
its delivery envelope (`id`, `sender`, `topic`, `created_at`) and adds the
compact body reference `{drawer_id, hash, first_200_chars}`; it does not inline
`content`. After choosing the message to consume, deliberately call
`get_drawer` with its `drawer_id` to retrieve the complete body. The `hash`
matches that body, and `first_200_chars` is a preview of its first 200 Unicode
characters (not bytes), or the whole body when it is shorter.

Message `drawer_id` values are opaque transport references, not content-derived
hashes. They are intentionally excluded from generic memory search; obtain one
only from the message envelope before calling `get_drawer`.

`full:true` additionally returns inline `content` for current rows, but exists
for compatibility rather than normal retrieval. Pre-016 legacy rows can have
`drawer_id:null`; because they cannot be dereferenced, their `content` remains
inline even under the default compact request.

In restricted MCP mode, `collab_recv` returns only the delivery envelope plus
`content_redacted:true` and `hash_redacted:true`. It omits the body, preview,
hash, and deterministic drawer ID, including when `full:true`, so the response
cannot reveal or fingerprint sensitive message content. A `get_drawer` lookup
is subject to the same sensitive-content redaction.

Every accepted message receives a durable immutable transport drawer in the
`collab-messages` room, while its queue row retains the delivery metadata and
`drawer_id` reference. Current retention does not make `collab-messages`
drawers deletion candidates, and its linked-collab reference guard recognizes
message drawers, so it does not orphan a message's transport reference.

**Telemetry:** the existing MCP response-sizing report already tags and groups
tool calls. The compact default is therefore observable as lower response
sizing for `collab_recv`, while deliberate `get_drawer` retrievals are reported
as their own tool calls; this introduces no collab-recv-specific metric, schema,
or report.

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

**Recovery-state fields.** Also returns `pending_failure`, `failed_from_phase`,
`recovery_phase`, `recovery_owner`, `recovery_origin_owner`,
`recovery_attempts`, and `total_recovery_attempts` (see "Failure + terminal"
above for the full semantics — `recovery_attempts` is the per-resume budget and
resets; `total_recovery_attempts` is the monotonic lifetime count and never
resets, so it is the field to read when judging whether a session is worth
continuing).

**`recovery_origin_owner`** names the agent that owned the turn the failure
interrupted. Control is **not** handed back to it: the recovery owner
(`recovery_owner`) completes the interrupted turn itself, and the phase's
normal completion event picks the next owner exactly as it would have from
the original owner. The field is provenance, not routing — it is the only
thing on this surface that distinguishes a completion event produced by a
delegated recovery owner from one produced by the phase's own expected
agent. Read it when auditing who actually did the work in a phase, not when
deciding who acts next.

**`pending_failure` is the
recovery-in-progress signal:** if it is non-null, `current_owner` was just
flipped by a recoverable `failure_report` rather than by a normal turn
advance — this is how an agent that reads `current_owner == <itself>`
distinguishes "it's simply my turn" from "I am the recovery owner for an
interrupted turn." A worker whose preconditions match `current_owner` should
check `pending_failure` as part of state discovery: when set, follow the
recovery-owner protocol above (inspect the preserved diff, run this phase's
gates, commit + push, send the phase's own normal completion event rather than
re-reporting the failure you were handed) before doing anything else. `coding_failure` is
`null` whenever `pending_failure` is set — see the distinction above.

#### Plan-by-reference contract

Accepted plan and task-list bodies are returned by reference to keep status
payloads bounded:

- **Plans:** `canonical_plan_ref` / `final_plan_ref` are always
  `{drawer_id, hash, plan_file_path}`. `plan_file_path` is parsed from the
  required leading marker and is null when absent. `verbose:true` remains an
  accepted compatibility argument, but never inlines either plan body.
- **Task lists:** `task_list_ref` is `{drawer_id, hash}`. With
  `include_task_list:true`, the `task_list` field repeats the same compact
  reference rather than inlining canonicalized JSON. New sessions can load the
  manifest with `get_drawer(task_list_ref.drawer_id)` and verify its hash.
- **Legacy sessions:** for a NULL plan drawer id, status reads the persisted
  message only to produce a body-free `{drawer_id:null, hash, plan_file_path}`
  reference. It never emits `canonical_plan` or `final_plan`.
- The `final` plan drawer stores the already-parsed plan text. Its `hash`
  verifies the on-disk plan file used by the task-list bridge.
- **Recall note:** accepted plan bodies are filed as drawers in the
  dedicated `collab-plans` room, and task lists in `collab-task-lists`, with a
  zero embedding (kept out of vector recall). The generic drawer FTS index
  still sees their content, so an unscoped keyword `search` can surface them.
  This is an accepted tradeoff for issue #90; excluding these rooms from
  default recall is tracked as a
  follow-up.
- **Retention note:** `collab-plans`, `collab-task-lists`, and
  `collab-checkpoints` are operational artifacts. Long-term project memory
  should be captured as compact summaries or KG facts. Use
  `ironmem memory gc --dry-run` before `--apply` to inspect stale operational
  drawer candidates; referenced plan/task-list drawers are skipped.

### `collab_approve`

Copilot-only shortcut for an `approve` review. The approver is the
session's copilot — the agent that is not its `pilot`, so Codex under the
default `pilot=claude` and Claude under `pilot=codex`. Any other agent is
rejected naming the one that may approve. Requires `content_hash` to
match the stored `canonical_plan_hash`.

### `collab_wait_my_turn` (long-poll)

Blocks server-side until the caller is the owner, an actionable post-claim
session-state change occurs, or `timeout_secs` elapses.

```json
{ "session_id": "...", "agent": "claude", "timeout_secs": 30 }
```

The response is a union: a settled full frame (the caller owns the turn, or an
**actionable post-claim session-state change** occurs) returns
`{ is_my_turn, phase, current_owner, session_ended }`. Relevant changes include
phase, owner, terminal/ended, and **recovery-state changes** such as a pending
failure or recovery ownership; an elapsed timeout with no relevant change
before the deadline returns exactly `{"unchanged": true}`. Default timeout
30s, max 60s. Agents loop on this instead of polling `status` on a fixed
interval.

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

### `collab_resume`

Restores a `CodingFailed` session back to its `failed_from_phase`. This is
a **separate MCP tool, not a `collab_send` topic**. See "Failure +
terminal" above for the recoverable-vs-terminal classification and the two
retry ceilings that can land a session in `CodingFailed` even from a
Tooling-classified failure.

```json
{ "session_id": "...", "agent": "codex" }
```

Eligible only when the session's stored `coding_failure` classifies
**Tooling** (one of the six recoverable prefixes, with a detail suffix),
`failed_from_phase` was recorded, **and** the lifetime recovery budget is
not exhausted. An ineligible call rejects with `NotResumable { reason }`:

- A **Terminal**-classified `coding_failure` (unrecognized cause,
  `branch_drift:`, or `subagent_failure:`) is never resumable — `reason`
  states this as a fact about the stored classification, never a guess about
  what happened during coding. A Tooling report that broke the per-resume
  retry ceiling remains Tooling and is resumable.
- A session whose `total_recovery_attempts >= MAX_TOTAL_RECOVERY_ATTEMPTS`
  (5) is not resumable regardless of classification. This is the stop on the
  resume→retry→resume loop: because `collab_resume` is agent-callable and
  allowlisted for unattended successors, without this check a Tooling
  failure could be resumed indefinitely.
- A session whose `failed_from_phase` is `NULL` predates this feature;
  `reason` says the session "predates resume support."

On success: `phase` is restored to `failed_from_phase`, `current_owner`
becomes the caller (`agent`), `coding_failure` clears, and the prior
terminal diagnostic moves into `pending_failure` for audit.
`recovery_attempts` resets to `0`, giving the restored turn a fresh
per-resume retry budget. `total_recovery_attempts` is **not** reset — it
carries the session's whole recovery history across every resume, and is
what eventually makes the session permanently non-resumable.
`failed_from_phase` itself is left set as a historical record. This path
is for when the retry ceiling was exceeded, or a fresh process wants to
pick a dead-but-recoverable session back up — it is not needed for the
normal in-flight recovery path (staying in-phase after a Tooling
`failure_report`), which never calls `collab_resume` since that session
never leaves its phase in the first place.

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

**What it does.** The server reads persisted session state and the one
logical-keyed current `collab-checkpoints` drawer for the session (falling back
to the newest legacy checkpoint only during rollout) and composes a deterministic,
model-free fenced markdown block (` ```ironrace-session-handoff `) — it
NEVER asks a model to summarize. This tool is a WRITE tool and is denied in
read-only / restricted MCP mode.

**Recovery-state lines.** The block mirrors the `collab_status` recovery
fields — `pending_failure`, `failed_from_phase`, `recovery_phase`,
`recovery_owner`, `recovery_origin_owner`, `recovery_attempts`, and
`total_recovery_attempts` — so a successor can route an interrupted recovery
turn, and judge how much recovery the session has already burned, from the
block alone. The two counters render as plain integers (`0` on legacy rows
whose columns are `NULL`); the four `Option` fields render an em-dash
placeholder when unset, like every other unset field in the block.

**Generation lease.** Each `(session_id, agent)` pair tracks an `active
generation` and a `pending_handoff_generation`. `session_handoff` issues (or
byte-identically reuses) a one-time `handoff_token` and sets
`pending_handoff_generation = active_generation + 1` **without** advancing
the active generation. A successor presents the `handoff_token` on its first
actor-bearing mutating/binding collab call (`collab_send`, `collab_recv`,
`collab_ack`, `collab_approve`, `collab_set_implementer`, `collab_set_pilot`,
`collab_register_caps`, `collab_wait_my_turn`, `collab_end`, `collab_resume`, or
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

> **Never spawn an unattended successor into the planning gate.** The
> exclusion is **every v1 planning phase**, not just the gate's own: if `phase`
> is `PlanParallelDrafts`, `PlanSynthesisPending`, `PlanCodexReviewPending`,
> `PlanClaudeFinalizePending`, or `PlanLocked` with no `task_list` sent, use
> the **Interactive phases** flow below instead. **Reason the list is this
> wide, so it cannot be narrowed without confronting it:** blind drafts,
> synthesis, the copilot's single review and the autonomous finalize turn run
> end to end with **no human checkpoint anywhere between them and the gate** —
> the dispatcher-owned gate at `PlanLocked` (see § Approval gates are
> reference-only, and `.claude-plugin/commands/collab.md` § v3 Bridge, step 0)
> is the only one left in v1 — so a successor spawned at *any* v1 planning
> phase drives straight through finalize into it. Dropping a phase from this
> list is only safe if some other human checkpoint sits ahead of the gate on
> that phase's path, and there is none. The whole reason the gate is the
> dispatcher's is that a one-shot process cannot prompt a human, and
> `claude -p` is exactly such a process: the successor would arrive at the gate,
> find no human to ask, and either stall forever or self-approve the only human
> checkpoint in the protocol. The cron fallback below re-fires every minute, so
> a self-approving successor is not a one-off. Mint the token, report
> `join collab <sid>` to the user, and stop — a session parked at the gate
> waiting for a human is the correct outcome, not a failure.

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
- `mcp__ironmem__collab_resume` — resume a tooling-class `CodingFailed` session
- `mcp__ironmem__session_handoff` — re-handoff if needed
- `mcp__ironmem__collab_status` — read session state
- `Bash(claude -p "join ironmem collab *":*)` — re-spawn a further successor if
  needed. Scope the wildcard to the known join-command form; avoid the broader
  `Bash(claude -p:*)`, which would let the successor spawn arbitrarily-prompted
  sub-agents.
- Git bash operations (`Bash(git commit:*)`, `Bash(git push:*)`, etc.) as
  needed for the implementation tasks the successor will perform.

`mcp__ironmem__collab_set_pilot` is deliberately **not** on this list: the
successor is spawned as `join ironmem collab <sid> with token <token>`, which
carries no `--pilot`, and `join` calls `collab_set_pilot` only for a `--pilot`
that was actually given. No successor path reaches it — and the server admits
the call only in `PlanParallelDrafts` before either draft lands, which the
guard above already routes to the manual flow.

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
`verdict`, `notes`, `comment`, or `subagent_report` field — the copilot reads
the diff and the approved task markdown in the repo at the
global review stage and forms its own judgment. This is the rule that prevents the
orchestrator from steering the reviewer's conclusion.

| Topic | Sender | Payload | Notes |
|---|---|---|---|
| `task_list` | `pilot` | `{"plan_hash","base_sha","head_sha","plan_file_path"?,"execution_mode"?,"tasks":[{"id","title","timebox_minutes","acceptance":[...]}]}` | `plan_hash` must equal `final_plan_hash`; `tasks` must contain **1–10** strictly ordered entries; each task requires `timebox_minutes <= 20` and ≥1 `acceptance` entry. An 11+ task issue must be split into child issues before this message is sent. Optional `plan_file_path` (repo-relative; no leading `/`; no `..` segments) points at the approved task markdown driving subagent execution. Optional `execution_mode` — see below. |
| `implementation_done` | `claude` or `codex` (per session `implementer`) | `{"head_sha"}` | In `CodeImplementPending` only. Fired once after the subagent batch completes and gates pass. Carries only `head_sha` — no prose, no subagent notes. |
| `review_fix_global` | `copilot` (or the counterpart agent as recovery owner under the delegated-completion override) | `{"head_sha"}` | In `CodeReviewFixGlobalPending` only. The copilot ran `/pr-review-toolkit:review-pr` on the raw post-implementation diff (no pre-clean by the pilot), used parallel fix subagents for confirmed partitionable findings, merged/cherry-picked the resulting fixes, and pushed the branch-level fix commit(s). |
| `review_local` | `pilot` (or the counterpart agent as recovery owner under the delegated-completion override) | `{"head_sha"}` | In `CodeReviewLocalPending` only. The pilot ran full or reduced audit of the copilot's `review_fix_global` commits + caught issues both agents missed, used parallel fix subagents for confirmed partitionable findings, merged/cherry-picked the resulting fixes, and pushed. |
| `final_review` | `pilot` (or the recovery owner under the delegated-completion override) | `{"head_sha","pr_url"}` | In `CodeReviewFinalPending` only. In normal flow the Claude submit worker opens the PR under the turn owner's identity; a Codex recovery owner opens it directly. The event carries the URL and advances directly to `CodingComplete`. `pr_url` must start with `https://` and be ≤2048 chars. |
| `failure_report` | current owner; off-turn `branch_drift:` may come from either agent; off-turn `codex_dispatch_failed:` may come only from Claude against a Codex-owned turn | `{"coding_failure":"<reason>"}` | Valid in any coding-active phase. Classifies **Tooling** (six recoverable prefixes, stays in-phase, `current_owner` flips) or **Terminal** (everything else — including `branch_drift:` — transitions to `CodingFailed`) — see "Failure + terminal" above. |

| `orphan_recovered` | current owner | `{"phase","recovered_sha","detail"}` | Valid in any phase. **Non-advancing**: the server records it and returns before building an event, so `phase`, `current_owner`, `recovery_attempts` and `total_recovery_attempts` are all unchanged. Sent *in addition to* the turn's normal completion event, never instead of it. Deliberately not a `failure_report`: a Tooling failure parks the phase, hands the turn over and spends a recovery attempt against a ceiling of 2 — but the sender here succeeded, having preserved work a previous turn left behind. The row is self-addressed with `status='recorded'` so it never appears in either agent's `collab_recv` inbox. `collab_status` reports the count as `orphans_recovered`. |

The `Sender` column above names the agent that owns the phase in the normal
flow. While a recovery is in flight, the delegated-completion override
accepts the **recovery owner** — the counterpart agent — sending that same
completion event in the original owner's place; see "Reporter and
recovery-owner protocol" under "Failure + terminal" above.

### `task_list` — `execution_mode` field

The optional `execution_mode` string field on the `task_list` payload selects
the implementation strategy for the batch phase. It is validated at send time
and exposed as a top-level `execution_mode` field in `collab_status` so both
agents can read it without re-parsing the canonicalized `task_list` JSON.

| Value | Behaviour |
|---|---|
| *(omitted)* | Default: subagent-driven. The implementer agent invokes `iron-build` (one subagent per task). |
| `"mechanical_direct"` | Single-task verbatim plan. The implementer applies the plan's bash/code blocks directly without invoking `iron-build`. **Currently honored by the Codex implementer only** — Claude's implement worker does not read `execution_mode` and always invokes `iron-build`. The field is cross-harness by design, so closing this gap means teaching Claude's implement worker to read `execution_mode` too, not dropping the field for Claude: both implementers honoring it is the intended end state. |

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
this mode when ALL four conditions hold — but note the value table above: the
field is a no-op whenever `implementer == "claude"` (the default), because
only the Codex implementer reads it today. Setting it is still correct; it
just does not change Claude-side behaviour yet.

1. The approved task markdown produced exactly one task (`### Task 1` only).
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
| `PlanCodexReviewPending` | `review` | v1 — the copilot's review of canonical |
| `PlanClaudeFinalizePending` | `final` | v1 — the pilot finalizes and sends autonomously; the dispatcher's gate is one phase later, at `PlanLocked` |
| `PlanLocked` | `task_list` | v1 → v3 hand-off, gated by the dispatcher's planning approval gate (see `.claude-plugin/commands/collab.md` § v3 Bridge, step 0) |
| `CodeImplementPending` | `implementation_done`, `failure_report` | v3 — single implementer turn after subagent batch |
| `CodeReviewFixGlobalPending` | `review_fix_global`, `failure_report` | v3 — the copilot runs `/pr-review-toolkit:review-pr` on the raw post-implementation diff, then fans confirmed fixes out to subagents |
| `CodeReviewLocalPending` | `review_local`, `failure_report` | v3 — the pilot audits the copilot's commits, then fans confirmed fixes out to subagents |
| `CodeReviewFinalPending` | `final_review`, `failure_report` | v3 — the pilot owns the final-review sender identity; the Claude submit worker opens the PR normally, while a recovery owner opens it directly |
| `CodingComplete` / `CodingFailed` | *(none — terminal; only `collab_end` accepted)* | |

`failure_report` is accepted from the current owner in any coding-active
phase. `branch_drift:` with real detail is also accepted off-turn from either
agent; `codex_dispatch_failed:` with real detail is accepted off-turn only
from Claude against a Codex-owned turn. A **Terminal**-classified report
transitions the session to `CodingFailed`; a **Tooling**-classified report
(one of the six recoverable prefixes, with detail — see "Failure + terminal"
above) instead keeps the session in its current phase and hands recovery to
the counterpart of the interrupted turn owner. All other topics are gated by
the owner recorded in the phase table above.

`collab_resume` is a separate MCP tool, not a `collab_send` topic, so it is
out of scope for this table — but it is the one way back into a coding
phase from `CodingFailed`, when the stored failure classifies `Tooling` and
`failed_from_phase` was recorded. See "Failure + terminal" above.

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
  `last_head_sha`, is the current branch already the session branch, AND is
  `git status --porcelain` empty? When all three hold, steps 3 (`git fetch`)
  and 5 (`git checkout` + `git reset --hard`) are skipped entirely. The
  `git cat-file -e` sanity check (step 4) still runs because the commit is
  already local. This avoids a network round-trip and a working-tree reset on
  the common case where the agent is already at the right SHA — for example,
  entering the batch-impl turn immediately after `task_list` is sent.
  Cleanliness is a condition of the fast path and not only of the reset:
  "HEAD at `last_head_sha`, branch correct, tree dirty" is precisely the state
  an in-container OOM leaves behind, and a two-condition fast path skips the
  reset — destroying nothing — only to build the next turn on top of the dead
  turn's unrecovered work. A dirty tree here takes the preservation path
  below.
- **Recovery turns preserve the diff.** If `pending_failure` is non-null and
  the current harness owns the session, it is recovering an interrupted turn.
  It must inspect the existing worktree and run that phase's gates before
  fetching, checking out, or resetting; ordinary pre-send synchronization may
  discard the reporter's uncommitted recovery diff. It then commits/pushes and
  sends the interrupted phase's normal completion event.
- **A clean worktree is an unconditional precondition of the reset.**
  Preservation must never be gated on `pending_failure` alone. A turn that
  dies hard — OOM, container kill, sandbox teardown — never sends a
  `failure_report`, so `pending_failure` stays null and the next dispatch
  correctly self-classifies as a *normal* turn. Immediately before
  `git reset --hard`, the harness must therefore require
  `git status --porcelain` to be empty regardless of `pending_failure`. A
  dirty worktree at that point is evidence of an unreported death, not of an
  empty turn: do not reset, and take the preservation path above instead.
  Without this, a normal turn destroys the only copy of the dead turn's
  uncommitted work with nothing downstream registering the loss.
  `scripts/check_collab_turn_templates.py` enforces this structurally
  (`check_reset_guards`) on every prompt that orders a reset.
- **Cleanliness also means "no unpushed commits."** `--porcelain` reports the
  working tree and the index; it is silent about work that was committed but
  never pushed, and `git reset --hard <last_head_sha>` discards those commits
  just as completely. Since `iron-build` commits per task, a turn killed
  partway through is likelier to leave committed work than unstaged work. The
  harness must therefore require `git rev-list <last_head_sha>..HEAD` to be
  empty alongside the porcelain check, and treat either one failing as the
  same unreported death.
- **The reset needs a checkout in front of it.** `git fetch` plus
  `git reset --hard` never moves the checkout, so a turn that inherits
  whatever branch its predecessor left behind resets *that* branch to the
  session head and pushes from it. Every phase that orders a reset also
  commits and pushes, so every one of them must check out the session branch
  first.
- **A recovery owner reviews the range it is about to send.** The review
  turns audit `base_sha..last_head_sha` as read from `collab_status`, but
  their completion event carries the *current* head. A recovery owner commits
  what it recovered, so its head moves past the recorded `last_head_sha` —
  and reviewing the recorded range while sending a head beyond it promotes
  those commits to session head with nobody having read them. After
  recovering, the review range head is the post-recovery `HEAD`.
- **Subagent orchestration** during `CodeImplementPending`. The pilot's
  finalize turn produces the approved task markdown, the dispatcher gates it
  at `PlanLocked`, and the bridge publishes it via `task_list`. The selected
  `implementer` then runs `iron-build` to dispatch fresh subagents per task. Each
  subagent runs TDD and commits on the branch. Per-subagent failures pause
  for triage; an unrecoverable failure surfaces as `failure_report` with
  `coding_failure: "subagent_failure: ..."`.
- **Implementation checkpoints** during `CodeImplementPending`. The
  implementer writes `ironrace-memory/collab-checkpoints` drawers before
  each task starts, after each task completes, on blocked failures, and
  after final gates pass, always with
  `logical_key: collab-checkpoint:<session_id>`. This replaces one
  logical-keyed current drawer while preserving cumulative
  `completed_task_ids`. On a fresh join at `CodeImplementPending`, the
  implementer reads that one logical-keyed current drawer and resumes
  from the first unfinished task instead of relying on transcript context.
- **Local gates** before Claude-owned code-changing turns
  (`implementation_done` in Claude-implementer mode, and `review_local` when
  Claude is the pilot): `cargo fmt --check`, `cargo clippy -D warnings`,
  `cargo test --workspace`. In Codex-implementer mode, Codex runs its own
  gates before sending `implementation_done`; likewise under `pilot ==
  "codex"` Codex runs its own gates before sending `review_local`. Any failure
  surfaces as `failure_report`; don't hide it.
- **Pushed-head proof** during `final_review`: the final-review compose worker
  does not re-run gates. It verifies a clean worktree, `HEAD == last_head_sha`,
  and local HEAD equal to the pushed upstream/origin branch head. If that proof
  fails, the worker blocks for branch-drift triage instead of burning another
  full gate run. The successful push from `review_local` is the gate evidence
  for this exact HEAD.
- **Review + fix tooling** during the copilot's `review_fix_global` (Codex
  under the default `pilot == "claude"`, Claude under `pilot == "codex"`):
  `/pr-review-toolkit:review-pr` runs as the final branch review pass over the
  compact `ironmem review-diff --repo <repo_path> --base <base_sha> --head
  <last_head_sha>` artifact alongside the approved task markdown
  when available. The copilot injects that artifact only on success; an error,
  feature-off build, or nonbeneficial artifact uses the exact raw fallback
  `git diff <base_sha>..<last_head_sha>`. On artifact success it does not retain
  or inject the complete raw diff. The artifact index supports focused source
  reads with `--expand-file <path> --hunk <ordinal>`; reviewers still inspect
  changed source and callers independently. Build this optional path with
  `cargo build --features headroom-compression`. The collab `base_sha` and
  `last_head_sha` define the review target; the copilot must not let the toolkit
  silently substitute a different base branch. The copilot treats the toolkit
  output as a read-only finding pass, independently verifies findings, then
  groups
  confirmed branch-level issues into non-overlapping fix clusters. For
  multiple independent clusters, the copilot creates one temporary worktree per
  cluster on a unique throwaway branch from the same review head and
  dispatches fix subagents in parallel; each subagent owns exactly one
  cluster and returns/commits only that cluster's edits. The copilot then merges
  or cherry-picks those fix commits back onto the collab branch, resolves
  conflicts, runs the required gates, commits/pushes the integrated result,
  and sends `review_fix_global`.
  If findings overlap or touch the same fragile area, the copilot fixes that
  cluster sequentially instead of forcing unsafe parallelism.
  `/ultrareview-local` is NOT used here — the pilot may run it later in full
  `review_local` mode. The copilot's judgment is expressed as commits, not prose.
- **Audit tooling** during the Claude-side `review_local` worker (normal flow
  when `pilot == "claude"`; under `pilot == "codex"` Codex's
  `collab-review-local.md` owns the equivalent audit and its own gates):
  Claude first runs an
  overlap-mode audit. Full mode invokes `/ultrareview-local` (code-reviewer +
  security-reviewer + architect + doc-reviewer in parallel) against the
  post-`review_fix_global` head, auditing the copilot's commits + catching
  code-quality issues both agents missed. Reduced mode is allowed when the
  copilot made no fix commit (`review_fix_global.head_sha` equals the preceding
  `implementation_done.head_sha`) or when the branch diff is docs/config-only.
  Reduced mode still audits the diff summary, changed files, and copilot commits
  for protocol drift, docs/config breakage, generated metadata inconsistencies,
  and security-sensitive configuration, and escalates to full
  `/ultrareview-local` on uncertainty or a substantive finding. Its PR mode
  attempts the same range artifact before `gh pr diff <N>` and its worktree
  mode attempts `ironmem review-diff --repo <repo_path> --worktree` before
  `git diff HEAD`; raw output is used only on error, feature-off, or
  nonbeneficial artifact. For conditional-reviewer trigger detection,
  `/ultrareview-local` also keeps a full raw diff only transiently, never
  injects or repeats it in reviewer prompts, then discards it after dispatch.
  Claude
  independently verifies the synthesized findings, groups confirmed
  CRITICAL/HIGH/MEDIUM findings into non-overlapping fix clusters, uses
  temporary worktrees on unique throwaway branches plus parallel fix subagents
  for independent clusters, merges or cherry-picks those fix commits back onto
  the collab branch, resolves conflicts, runs the required gates,
  commits/pushes the integrated result, and sends `review_local`. Overlapping
  or risky findings are fixed sequentially by the review worker.
- **Review-artifact measurement:** capture the normal `ironmem report --json`
  data with repeatable `python3 scripts/collab_baseline.py capture
  --review-artifact PHASE=PATH` inputs after saving each successful artifact.
  The stable footer records `source_bytes`, `artifact_bytes`,
  `source_estimated_tokens`, and `artifact_estimated_tokens`; capture rejects
  malformed or non-shrinking profiles, while repeated profiles are retained for
  comparison.
- **Shortcut ancestry validation** during shortcut-started
  `review_fix_global` and `review_local`: the server shells out narrowly
  to `git merge-base --is-ancestor` to distinguish a true descendant
  check from operational git failures, and only applies that validation
  when `task_list` is still unset. Both the copilot's `review_fix_global` push
  and the pilot's `review_local` audit-push must descend from the prior
  `last_head_sha`.
- **PR creation** during `final_review`: after pushed-head proof passes, the
  Claude submit worker resolves the integration branch from the remote default,
  falling back to the first existing `origin/main`, `origin/master`, or
  `origin/trunk`; it does **not** require that branch to contain `base_sha`.
  Missing containment is normal for a branch cut from an unpublished commit:
  the worker lists the pre-range commits in the PR body, then runs
  `gh pr create --base <base_branch> ...` and sends the URL inline with the
  `final_review` event under the current owner's identity. There is no separate
  `pr_opened` turn.
- **PR creation is scoped by protocol ownership, not by tooling.** Codex may
  run any `gh`, git, or GitHub API operation it needs; nothing about the
  tooling is off-limits. What is restricted is *who is allowed to actually
  execute `gh pr create`*. Codex runs `gh pr create` itself only when it
  owns `CodeReviewFinalPending` under the recovery override — i.e. all three
  of `pending_failure` non-null, `current_owner == "codex"`, and
  `recovery_phase == "CodeReviewFinalPending"` (see `collab-recovery.md`).
  Outside recovery, `gh pr create` is always run by the Claude-side
  `collab-turn-submit.md` worker — even under `pilot == "codex"`. There,
  Codex owns the `CodeReviewFinalPending` turn and composes the PR
  title/body, but its own prompt (`collab-final-review.md`) explicitly sends
  nothing and opens no PR ("This turn sends nothing and opens no PR... the
  orchestrator dispatches the submit worker with your staged artifact, and
  that worker owns PR creation and the completion send"); the orchestrator
  then dispatches `collab-turn-submit.md` to run `gh pr create` and send
  `final_review` as `$SENDER=codex` on Codex's behalf. So "who is the
  protocol turn-owner / send identity" and "who literally executes
  `gh pr create`" diverge under `pilot == "codex"`: Codex owns the send
  identity, but Claude's submit worker still makes the `gh` call. Codex
  should still avoid gratuitous PR probes in its ordinary phases (`gh pr
  list`, `git ls-remote refs/pull/*`) — they add an `api.github.com`
  reachability dependency that was observed as a fragility in practice —
  but that is a robustness preference, not a prohibition.

  **Why the old blanket ban was a defect.** The previous rule said Codex
  must never call any PR-related operation, full stop. Recovery can hand
  `CodeReviewFinalPending` to Codex, and `parse_final_review_event` requires
  a `pr_url` starting with `https://`. Under the blanket ban, a recovering
  Codex had exactly two moves: fabricate a URL — permanently corrupting
  `sessions.pr_url` and the downstream task-outcome metrics — or re-report
  and burn the retry budget down to `CodingFailed`. Both are wrong outcomes
  produced by the rule itself.

- **Never fabricate or guess a `pr_url`.** The server validates the scheme
  and length; it cannot tell a real PR from an invented one, so a made-up
  URL is accepted and becomes permanent session state. If a PR genuinely
  cannot be created, send `failure_report` with the prefix that describes
  what actually failed (`network_failed:` or `sandbox_denied:` as
  applicable) rather than inventing a URL. A recoverable failure report
  costs one retry attempt; a fabricated URL corrupts the record forever.
- **Plan Mode** on the dispatcher's side is entered before only one gate: the
  locked plan at `PlanLocked` (v1), where the dispatcher presents the
  approved task plan for approval before dispatching the `task_list` bridge.
  Canonical synthesis and the `final` send both run autonomously — the plan
  is already on the wire by the time the gate fires. The `task_list` send
  itself mechanically parses that already-approved markdown; it is not an
  extra task-planning handoff and not a second Plan Mode gate. The
  `final_review` PR creation is autonomous (no gate). Codex never enters Plan
  Mode; under `pilot == "codex"` the dispatcher terminal still takes this
  gate, since the dispatcher is always Claude.

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
`$TOPIC`, `$ARTIFACT_REF`, `$ARTIFACT_HASH`, `$MODE`, `$SENDER` where that template uses
them). The worker
calls the ironmem MCP tools directly and reads/writes artifacts via drawers and
files. **Full artifacts — plans, diffs, review reports, PR bodies — never
transit the orchestrator.**

**Anti-puppeteering.** The orchestrator passes ONLY the resolved template. It
never appends an inline recap, a state summary, or "what to conclude." Each
worker discovers state for itself via its own `collab_status` / `collab_recv` /
drawer fetches. This structurally removes the channel the orchestrator could
otherwise use to steer a worker's judgment (mirrors the v3 design that keeps
the pilot from steering the copilot's review).

**Verdict contract.** The worker's final message is at most three lines
(`result:` / `ref:` / `blocker:`). The orchestrator stores only this verdict;
it does not ingest the body of whatever the worker produced.

### Model tiers + fail-closed

Workers are dispatched at one of three tiers. Fable is OFF, so planning and
review both run on Opus; mechanical turns run on Sonnet/default. Codex uses the
explicit phase-based policy below rather than inheriting the caller's personal
model default.

| Tier | Turns | Dispatch |
|---|---|---|
| `planning` | `draft`, `canonical` (synthesis), `final` (finalize) | `Agent(model=opus)` at max effort |
| `review` | `review_local`, `final_review` | `Agent(model=opus)` |
| `mechanical` | `task_list`, `code-implement` controller, `submit` | `Agent(model=sonnet)` / default |

### Codex model policy

Codex dispatch is explicit and phase-based. These are the background
dispatcher's defaults for collab's own protocol turns:

| Codex work | Model | Effort |
|---|---|---|
| Implementation controller/workers | `gpt-5.6-luna` | `max` |
| Exploration, docs, and mechanical work | `gpt-5.6-luna` | `medium` |
| Planning and normal review | `gpt-5.6-terra` | `high` |
| Architecture/security escalation | `gpt-5.6-sol` | `high` |

The `Implementation controller/workers` row governs the controller collab
dispatches into `CodeImplementPending`. Per-task subagent models inside that
run come from `iron-build`'s `**Tier:**` ladder, not from this table — see
§ Reviewer model selection for the divergence this row still carries.

`gpt-5.6-sol` is an escalation tier, not the default. Use it when a
discovered architecture, security, or other high-risk issue needs additional
judgment; do not default to Sol or to Sol `max`. A protocol turn must pass its
model and effort explicitly so a user's personal Codex default cannot silently
change the collaboration behavior.

"Max effort" is the harness thinking-budget mechanism. **Fail-closed rule: if
the harness cannot select the requested tier for a planning or review dispatch,
ABORT the turn and surface to the user — never silently fall back to a lower
tier.**

### Approval gates are reference-only

The only planning user gate is the locked plan, and the dispatcher takes it
at `PlanLocked` — after `final` is on the wire, before `task_list` is
dispatched (see `.claude-plugin/commands/collab.md` § v3 Bridge, step 0):

1. **Compose worker** (`collab-turn-plan-finalize.md`, dispatched at
   `PlanClaudeFinalizePending`) writes the final iron-build-compatible task
   markdown to `docs/iron/plans/...`, stages the exact markdown in a drawer,
   and returns `{drawer_id, plan_file_path, ≤3-line summary}`.
   `collab-turn-submit.md` sends `final` from that artifact immediately,
   autonomously — composing and sending `final` is not the gate. It never
   re-authors: if the artifact cannot be fetched, the submit worker does not
   send the protocol topic at all. For `final_review` (coding-active) it
   sends a `failure_report`; for the v1 `final` planning topic the state
   machine rejects `failure_report`, so it aborts with a `blocker:` verdict
   instead.
2. **Orchestrator gate**, at `PlanLocked`, surfaces ONLY
   `drawer_id + plan_file_path + summary` for the user's approval — never the
   full plan body.
3. **On approval**, the orchestrator dispatches `collab-turn-task-list.md`
   (§ v3 Bridge), which reads `final_plan_ref`/`final_plan_hash` by
   reference, verifies the file's SHA-256 against the approved hash, and
   sends `task_list`. **Drawer immutability is the integrity anchor:**
   drawers are append-only, so the approved `drawer_id`'s content cannot
   change — the ref itself guarantees the user approved exactly what gets
   parsed, and no hash recompute is needed (a cross-worker recompute would be
   both redundant and non-reproducible across readback/encoding). **On
   rejection**, the orchestrator does not send `task_list`; it offers
   `collab_end` instead — legal precisely and only at `PlanLocked`
   pre-`task_list`, the one clean abandon point in the session.

The same compose→submit pattern still drives the autonomous v3 `final_review`
PR creation (the orchestrator dispatches the submit worker directly, since the
diff already passed `review_fix_global` + `review_local` and a PR is editable
and unmerged after creation) — that turn runs gateless, with no dispatcher
approval step. Post-plan-lock work runs autonomously apart from the one gate:
the `PlanLocked` bridge's **parse** stays mechanical — it parses/submits
`task_list` from the approved markdown and never re-plans — while its
**dispatch** is exactly what the gate holds.

### v3 bridge (PlanLocked → CodeImplementPending) — worker-owned

The bridge's **parse** is worker-owned; its **dispatch** is gated. The
orchestrator does NOT call any separate plan-expansion skill, does NOT read a
plan body from status, and does NOT build the `task_list` manifest — it
dispatches `collab-turn-task-list.md` once. But before it dispatches anything,
it takes the **dispatcher-owned planning approval gate** at step 0 (§ Approval
gates are reference-only; `.claude-plugin/commands/collab.md` § v3 Bridge,
step 0): with `phase == PlanLocked`, `final_plan_ref` set and no `task_list`
sent, it enters Plan Mode and gets user approval, surfacing only
`{drawer_id, plan_file_path, ≤3-line summary}`. This gate is the dispatcher's
and no worker's — a `codex exec` one-shot cannot prompt a human, and under
`pilot == "codex"` the finalize turn belongs to Codex. On rejection it does not
send `task_list` and offers `collab_end` instead.

The worker reads `final_plan_ref`/`final_plan_hash`, obtains
`plan_file_path` from the reference, verifies the file's SHA-256 against the
approved hash, parses each `### Task N:` heading into
`{id,title,timebox_minutes,acceptance}`, and sends `task_list`. It returns a
blocker and sends nothing if `$SENDER` is absent or disagrees with
`collab_status.current_owner`, the plan file is missing or its hash differs, the
plan has zero or more than 10 tasks, any task is missing acceptance criteria,
missing `Timebox: <=20 minutes`, or is sized above 20 minutes. An 11+ task plan
must be decomposed into child issues; it must not enter coding. PlanLocked is
pre-coding, so `failure_report` is not valid in
this bridge.

**If the worker returns `blocker:`, the bridge is over — report it and exit
the loop.** The orchestrator must not re-dispatch the worker or fall back
through the loop into the step-0 gate. The plan is immutable at `PlanLocked`
(append-only drawer, file pinned to `final_plan_hash`), so a re-dispatch
re-reads the same bytes and returns the same blocker, while the loop still
sees `phase == PlanLocked` with `task_list` unsent and re-prompts the human
with a plan nobody can change — an unbounded re-approval loop. Report the
blocker verbatim and offer `collab_end`, legal precisely and only at
`PlanLocked` pre-`task_list`, exactly as on rejection at the gate; the work has
to be re-cut in a new session. `TaskListBridge` in
`benchmarks/abeval/src/collab_driver.rs` encodes the same rule, mapping a
bridge `blocker:` to `DriveError::Invalid` rather than retrying.

Only the worker's ≤3-line verdict crosses the orchestrator boundary; the plan
markdown and manifest JSON never do.

### Measurement gate

The dispatch design targets **orchestrator context growth ≤ ~300 tokens per
protocol turn**, since the orchestrator ingests only a ≤3-line verdict per
worker and never the produced artifacts. This is measured via occupancy
sampling — the same metrics instrumentation tracked under #82–#83.

### Worker templates

The ten per-turn worker templates live under `.claude-plugin/prompts/`:

- `collab-turn-plan-draft.md` — `PlanParallelDrafts` blind draft
- `collab-turn-plan-synthesis.md` — `PlanSynthesisPending` canonical
- `collab-turn-plan-review.md` — `PlanCodexReviewPending` copilot plan review
- `collab-turn-plan-finalize.md` — `PlanClaudeFinalizePending` final
- `collab-turn-task-list.md` — `PlanLocked` bridge (mechanical `task_list` submit)
- `collab-turn-code-implement.md` — `CodeImplementPending` batch (Claude implementer)
- `collab-turn-review-fix-global.md` — `CodeReviewFixGlobalPending` copilot review + fix
- `collab-turn-review-local.md` — `CodeReviewLocalPending` `/ultrareview-local` audit
- `collab-turn-final-review.md` — `CodeReviewFinalPending` PR-body compose
- `collab-turn-submit.md` — generic submit-by-ref + PR create

Codex has a matching phase prompt under `.codex-plugin/prompts/` for each of
the nine protocol turns above that carry a phase — every template except
`collab-turn-submit.md`. That omission is by design, not a gap: in the
normal flow, Claude's orchestrator loop is the one that dispatches the
submit turn regardless of pilot (only `$SENDER` is parameterized, from
`collab_status.current_owner`), so a Codex-side submit template would never
be invoked there and would just rot unused.

Routing through `collab-turn-submit.md` is not universal, though. Only the
two compose turns — `collab-plan-finalize.md` and `collab-final-review.md` —
stage an artifact and hand the send back to Claude's submit worker. Under
normal Codex-pilot ownership, `collab-plan-synthesis.md` and
`collab-review-local.md` send their own completion events (`canonical` and
`review_local`) directly. `collab-recovery.md` sends directly too: it handles
the rare recovery override that delegates `CodeReviewLocalPending` or
`CodeReviewFinalPending` to Codex, where Codex sends the completion event
(including opening the PR for `final_review`) itself.

The Claude-side dispatch tables and the authoritative tier matrix live in
`.claude-plugin/commands/collab.md`; this section and that command file must
stay in lockstep with the Codex command/prompt surface (see the headers in
`.codex-plugin/prompts/collab-*.md`).

## Autonomous Planning Loop

**Claude runs the single control loop.** Codex CLI sessions are one-shot:
each Codex-owned turn is dispatched inline via background `codex exec`
(see Implementation Notes § Background `codex exec` dispatch), reads
state, sends exactly one protocol message, and exits. There is no
symmetric long-running polling loop on the Codex side — Claude polls,
Claude dispatches. A single bounded `wait_my_turn` call at invocation
start (as in the matching `.codex-plugin/prompts/collab-*.md`) is permitted to bridge
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
| `PlanSynthesisPending` | synthesize `canonical` and send autonomously (no Plan Mode) | wait (not polling), unless `pilot == "codex"`: one-shot bg-exec synthesizes and sends `canonical` via `collab-plan-synthesis.md`, then exits |
| `PlanCodexReviewPending` | wait, unless `pilot == "codex"`: Claude is the copilot and owns this turn — dispatch `collab-turn-plan-review.md` (review/opus) and send `review` | one-shot bg-exec: send `review` (or `approve` shortcut), exit — only when `pilot == "claude"`, which makes Codex the copilot |
| `PlanClaudeFinalizePending` | compose and send `final` autonomously — no Plan Mode here | wait, unless `pilot == "codex"`: one-shot bg-exec composes and stages the final plan via `collab-plan-finalize.md`, then exits |
| `PlanLocked` | dispatcher enters Plan Mode, gets user approval for the locked plan (reference-only), then dispatches the `task_list` bridge on approval or offers `collab_end` on rejection; exit loop otherwise | n/a |

Phase → action (v3):

| Phase | Claude does | Codex does |
|---|---|---|
| `PlanLocked` (post-final) | enter Plan Mode, surface only `{drawer_id, plan_file_path, ≤3-line summary}` for the locked plan; on approval, dispatch one mechanical `collab-turn-task-list.md` worker with `$SENDER=<collab_status.current_owner>`, which parses the approved final markdown and sends `task_list` as the pilot; on rejection, offer `collab_end` instead | n/a — the Claude-side worker performs the parse and send under the pilot's identity even when `pilot == "codex"` |
| `CodeImplementPending` (implementer=claude) | dispatch `collab-turn-code-implement.md`; worker searches checkpoints, runs `iron-build`, gates, checkpoints, and sends `implementation_done{head_sha}` | wait |
| `CodeImplementPending` (implementer=codex) | dispatch Codex via bg-exec; poll | one-shot bg-exec: search implementation checkpoints, resume/run `iron-build`, checkpoint every task boundary, emit `implementation_done{head_sha}`, exit |
| `CodeReviewFixGlobalPending` | dispatch Codex via bg-exec and poll — **only when `pilot == "claude"`**, which makes Codex the copilot. Under `pilot == "codex"` Claude is the copilot and owns this turn: dispatch `collab-turn-review-fix-global.md` (review/opus) and send `review_fix_global`. Dispatching Codex into a phase it does not own makes its own ownership guard reject and exit, which the dispatcher then misreads as a spurious `codex_dispatch_failed:` that burns a recovery attempt | one-shot bg-exec: run `/pr-review-toolkit:review-pr` on the raw post-implementation diff, partition confirmed branch-level issues, fan out independent fix subagents in temporary worktrees on unique throwaway branches, merge/cherry-pick fixes back, send `review_fix_global`, exit |
| `CodeReviewLocalPending` | dispatch `collab-turn-review-local.md`; worker runs full or reduced `review_local` audit, partitions confirmed CRITICAL/HIGH/MEDIUM findings, fans out independent fix subagents in temporary worktrees on unique throwaway branches, merges/cherry-picks fixes back, and sends `review_local` | wait, unless `pilot == "codex"`: one-shot bg-exec runs the local audit, fans out its fixes, sends `review_local`, and exits |
| `CodeReviewFinalPending` | dispatch `collab-turn-final-review.md` compose worker for pushed-head proof + PR body, then dispatch `collab-turn-submit.md` **directly** (no gate) to `gh pr create` (ready PR) and send `final_review{pr_url}` | wait, unless `pilot == "codex"`: one-shot bg-exec drafts the PR title/body and stages it via `collab-final-review.md`, then exits |
| `CodingComplete` / `CodingFailed` | exit loop | n/a |

**Worktree cleanup reminder on `CodingComplete`.** The session's lifecycle
ends here, before a human merges the PR on GitHub — collab has no way to
observe the merge, so it cannot clean up automatically. If `start` created an
isolated worktree for this session (check: `git rev-parse --git-common-dir`
differs from `git rev-parse --git-dir` in `repo_path`), the exiting loop
includes a line in its final report to the user naming the worktree path and
the cleanup command (`git worktree remove <path>`, once the PR merges).
Cleanup is never run automatically — the branch/worktree must survive until
the PR is actually merged.

### Plan Mode integration (dispatcher)

The dispatcher (always Claude — § Runtime Model → Roles) enters harness Plan
Mode at **exactly one gate**, matching the command-file invariant bullet:

1. **`PlanLocked` pre-`task_list`.** After the pilot's `final` send has
   already locked the plan (`final_plan_ref` set, no `task_list` sent yet),
   the dispatcher presents the locked plan for approval, surfacing only
   `{drawer_id, plan_file_path, ≤3-line summary}` — never the plan body (see
   `.claude-plugin/commands/collab.md` § v3 Bridge, step 0). It must contain
   1–10 tasks, each timeboxed to 20 minutes or less; a larger plan must have
   been split into child issues
   before `final` was sent, since `PlanLocked` makes the plan immutable. On
   approval, the dispatcher dispatches the `task_list` bridge; on rejection,
   it offers `collab_end` instead of sending `task_list`.

Every other step runs autonomously, including the `final` send itself: the
blind `draft` send (from `/collab start`), canonical synthesis, the copilot's
one plan-review pass, the pilot's `final` send, the v3 `task_list` send
(mechanical parse/submit from the approved markdown, gated only by the
dispatcher's approval one phase earlier), and the v3 `final_review` PR
creation (auto-dispatched after the diff passes `review_fix_global` +
`review_local`). Codex never enters Plan Mode — it posts drafts, reviews, and
global fixes directly; under `pilot == "codex"` the dispatcher terminal still
takes the one `PlanLocked` gate, since the dispatcher is always Claude
regardless of pilot.

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

Claude's behavior on receiving this is specified executably in
`.claude-plugin/commands/collab.md` § `start` — this document does not
restate it. In summary: reuse the current branch when it is already an
isolated one; otherwise derive `collab/<slug>` from the task, uniquify it
against local and `origin` refs, and create a worktree for it — borrowing
only the directory-priority list from `iron-build`'s *Workspace* section, and
defaulting to `.worktrees/` rather than asking. Two invariants are
load-bearing and are stated in full there: collab
must never stop to ask the user for a path or branch name, and `main` /
`master` / `trunk` must never be recorded as the collab branch — the `branch`
field is fixed at `collab_start` time with no update API, so a recorded
`main` sends every later turn's checkout, hard-reset, and push straight at
`main`, bypassing PR review entirely.

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
| `repo_path` | `git rev-parse --show-toplevel`, **unless** `start` created a worktree (see below), in which case the worktree's absolute path |
| `branch` | **`start`, already on a non-default branch:** that branch, as-is. **`start`, on `main`/`master`/`trunk`/detached HEAD:** a newly created `collab/<task-slug>` branch in a new isolated worktree (see § Starting a session) — never `main`/`master`/`trunk` itself. **`join`:** the branch already recorded on the session (`collab_status.branch`); do not create another. |
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

Claude: resolves repo_path; current branch is `main`, so creates an
        isolated worktree at `.worktrees/collab/design-marketing-landing-page`
        on a new branch of the same name; initiator=claude.
        start → s_abc.
        Draft sent autonomously — no Plan Mode. Owner flips to codex.
Claude: dispatches Codex via background `codex exec` with the resolved
        Codex prompt. Begins polling collab_status + BashOutput.
Codex (bg-exec):
        reads status → task is "design marketing landing page".
        Submits one draft. Exits.
Claude: poll observes owner=claude, phase=PlanSynthesisPending,
        review_round=0. recv → sees Codex's draft. Synthesizes and sends
        canonical autonomously.
Claude: dispatches Codex again via bg-exec for the review.
Codex (bg-exec):
        reads canonical, returns verdict=request_changes. Exits.
Claude: poll observes phase=PlanClaudeFinalizePending. Finalize turn is
        autonomous — no Plan Mode here. Dispatches the finalize worker,
        which splits tasks to 20 minutes or less, routes any 11+ task scope
        into child issues, composes the final iron-build-compatible task
        markdown, and sends final. Phase now PlanLocked.
Claude: dispatcher takes the single planning gate here — **enters Plan
        Mode**, surfacing only {drawer_id, plan_file_path, ≤3-line summary}
        for the locked plan (never the plan body). User approves. Dispatches
        the task_list bridge worker, which parses the approved markdown and
        sends task_list. Loop exits (or continues into v3 coding).
```

Codex's single `request_changes` pass moves directly to
`PlanClaudeFinalizePending`, where the pilot composes and sends `final`
autonomously. The dispatcher's last word comes one phase later, at the
`PlanLocked` gate. Canonical synthesis and the `final` send are both
autonomous; only the locked plan is user-gated.

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
rather than via the synchronous `mcp__codex__codex` MCP tool. This covers
`PlanParallelDrafts`, Codex-normal-pilot `PlanSynthesisPending`,
`PlanCodexReviewPending`, Codex-normal-pilot `PlanClaudeFinalizePending`,
`CodeReviewFixGlobalPending`, Codex-normal-pilot `CodeReviewLocalPending`,
Codex-normal-pilot `CodeReviewFinalPending`, recovery-owned local/final review,
and `CodeImplementPending+codex`. The full
procedure (prompt file selection, reasoning flag, wait loop, termination
conditions, and failure handling) is documented in the Claude-side
dispatcher prompt (`.claude-plugin/commands/collab.md`, section "Codex
handoff — background `codex exec`").

**Why all phases.** Background exec avoids the MCP cold-start overhead
that dominated latency in smoke testing (`PlanCodexReviewPending` hung
24+ min; `CodeReviewFixGlobalPending` took 171s via synchronous MCP).
The dispatch shape is now uniform across all Codex turns; only the prompt
file and the explicit model/effort override vary by phase.
`CodeImplementPending+codex` uses `collab-batch-impl.md` and
`-m gpt-5.6-luna -c model_reasoning_effort=max`; `PlanParallelDrafts`,
`PlanSynthesisPending`, `PlanCodexReviewPending`, `PlanClaudeFinalizePending`,
`CodeReviewFixGlobalPending`, `CodeReviewLocalPending`, and
`CodeReviewFinalPending` use their matching `collab-*.md` phase prompts with
`-m gpt-5.6-terra -c model_reasoning_effort=high`; delegated
`CodeReviewLocalPending` or `CodeReviewFinalPending` recovery uses
`collab-recovery.md` with that same review setting. A discovered architecture or security issue may
escalate a subagent to `gpt-5.6-sol` at high effort, but the parent protocol
dispatch remains on its phase default.

**Sandbox: every Codex-owned dispatch passes `-s danger-full-access`.** Both
launch lines are:

```bash
# CodeImplementPending+codex:
cd <repo_path> && codex exec -m gpt-5.6-luna -c model_reasoning_effort=max -s danger-full-access - < /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1

# All other Codex-owned phases:
cd <repo_path> && codex exec -m gpt-5.6-terra -c model_reasoning_effort=high -s danger-full-access - < /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1
```

The flag is unconditional — never phase-, model-, or worktree-topology-
dependent. Codex runs unsandboxed by explicit choice, because the sandbox
breaks the protocol. A collab session normally runs from a linked worktree,
whose `.git` is a file pointing at `<main-repo>/.git/worktrees/<name>/`; that
per-worktree gitdir and the shared object/ref database Codex's `commit`/`push`
turn writes to both live outside any workspace-scoped root, so a
workspace-write sandbox denies `git commit` outright. Denials are also not
limited to the filesystem: under workspace-write, `cargo test --workspace`
failed the daemon/doctor tests with "Operation not permitted" because Unix
domain socket creation was denied, and no set of extra writable roots
(`--add-dir` or otherwise) can grant that capability. An earlier
`--add-dir "<common-gitdir>"` workaround addressed only the git-metadata half
of the problem and is superseded by this flag; do not reintroduce it.

**What the flag actually costs.** The decision stands, but the trade is not
free, and the boundary given up is *not* agent-vs-user — it is
agent-vs-**untrusted content**. Codex is dispatched by the user against the
user's own repo, so nothing here protects the user from their own agent. What
the sandbox would have contained is content the agent *reads*: Codex's
`review_fix_global` turn runs `/pr-review-toolkit:review-pr` over PR diffs and
review comments, both of which a third party can author. Prompt-injected
instructions in that material execute with full local filesystem and process
access — read any file the user can read, write anywhere, spawn anything.
`danger-full-access` additionally removes any restriction on **network
egress**, so injected content can also exfiltrate what it reads; the sandbox
argument above is about writes and sockets and never covered egress at all.

**Operational rule.** Do not run a collab session against a branch or PR whose
diff or review comments come from an untrusted author. Collab is for work
authored by the operator and their own agents. Reviewing third-party
contributions needs a sandboxed, egress-restricted review path that this
protocol does not currently provide.

#### Fallback: synchronous `mcp__codex__codex` MCP

When `codex` is not on PATH, the dispatcher falls back to synchronous
`mcp__codex__codex` for any phase. The prompt-file selection matrix is
unchanged; only the transport differs.

1. Register `codex mcp-server` with Claude Code (once):
   ```bash
   claude mcp add codex codex mcp-server
   ```
2. Claude expands the Codex prompt locally — `codex mcp-server` does
   **not** resolve interactive slash commands, even though Codex now has
   `.codex-plugin/commands/collab.md`. Passing a raw
   `/collab join <sid>` string through the MCP transport would make Codex
   treat it as ordinary user text and go off-script. Read the appropriate
   phase prompt file (`.codex-plugin/prompts/collab-plan-draft.md` for
   `PlanParallelDrafts`, `collab-plan-synthesis.md` for Codex-normal-pilot
   `PlanSynthesisPending`, `collab-plan-review.md` for
   `PlanCodexReviewPending`, `collab-plan-finalize.md` for Codex-normal-pilot
   `PlanClaudeFinalizePending`, `collab-global-review.md` for
   `CodeReviewFixGlobalPending`, `collab-review-local.md` / `collab-final-review.md`
   for Codex-normal-pilot local/final review, `collab-recovery.md` for delegated
   `CodeReviewLocalPending` / `CodeReviewFinalPending`, or `collab-batch-impl.md` for
   `CodeImplementPending+codex`), substitute `$ARGUMENTS` with
   `join <session_id>`, and call:
   ```json
   {
     "name": "mcp__codex__codex",
     "arguments": {
       "prompt": "<resolved prompt text>",
       "cwd": "<repo_path>",
       "config": {
         "model": "gpt-5.6-luna",
         "model_reasoning_effort": "max",
         "sandbox": "danger-full-access"
       }
     }
   }
   ```
   Select `config.model` and `config.model_reasoning_effort` from the Codex
   model policy and dispatch matrix. For `CodeImplementPending+codex` use
   `gpt-5.6-luna` at `max`; for planning and normal review use
   `gpt-5.6-terra` at `high`. Do not omit the model override or inherit the
   caller's personal default. `config.sandbox` is always
   `"danger-full-access"`, matching the CLI launch lines — the MCP transport
   gets no different sandbox treatment.
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
1778971814.91 t2_codex_dispatched phase=PlanCodexReviewPending round=1
1778971990.43 t3_codex_returned phase=PlanCodexReviewPending round=1
1778971814.93 t4_phase_advanced phase=PlanClaudeFinalizePending round=1
1778971990.99 t8_pr_created phase=CodeReviewFinalPending https://github.com/.../pull/123
```

**Required fields.** `phase=<phase>` and `round=<N>` are required on every
Codex-owned dispatch/return event (`t2_codex_dispatched`,
`t3_codex_returned`, `t6_codex_review_dispatched`,
`t7_codex_review_returned`) and on `t4_phase_advanced`. For
`t4_phase_advanced`, `phase=` is the new destination phase and `round=`
is the same dispatch round being watched by the wait loop (for
example, `round=1` for the single v1 plan review or the global review
phase). Events that fire exactly once per session
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
| `t3_codex_returned` | `phase=` `round=` | Immediately after a settled bg-exec wait reports success for PlanParallelDrafts, PlanCodexReviewPending, or CodeImplementPending+codex |
| `t4_phase_advanced` | `phase=` `round=` | Every time a settled wait observes a new phase (destination phase goes in `phase=`, NOT in the event name; round is the same dispatch round being watched by the wait loop) |
| `t5_review_local_sent` | `phase=CodeReviewLocalPending` | After `collab_send(topic="review_local")` returns |
| `t6_codex_review_dispatched` | `phase=` `round=` | Immediately before launching background `codex exec` for `CodeReviewFixGlobalPending` |
| `t7_codex_review_returned` | `phase=` `round=` | Immediately after a settled bg-exec wait reports success for `CodeReviewFixGlobalPending` |
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

### Event-driven Codex background waits

For a Codex-owned background `codex exec` phase, Claude waits with
`collab_wait_my_turn(session_id, "claude", 60)`, not a fixed-interval
`collab_status` poll. The wait response is the compact union documented
above: exactly `{"unchanged": true}` is a 60-second timeout with no relevant
change before the deadline; every other response is a settled full frame from an
**actionable post-claim session-state change**. Relevant changes include phase,
owner, terminal/ended, and **recovery-state changes** such as a pending failure
or recovery ownership, even if Codex remains the owner.

On an unchanged timeout, Claude immediately starts the next wait without a
`collab_status` call or an idle user update (except that it first performs the
single bounded background-output drain below and checks the still-required
process-exit and 600-second hang conditions). On a settled wake, it calls
`collab_status` once and uses the normal success, recovery, terminal,
process-exit, and hang handling in that order. If the changed phase remains
Codex-owned, Claude immediately selects and launches the next Codex prompt;
the prior process's normal exit is not a silent-dispatch error. This preserves
phase-specific timing events, recovery/resume routing, and the 600-second
no-progress safeguard; progress is a phase change or new stdout.

Claude reads background stdout once per wait wake. New output is relayed as
one `[codex bg]` batch, using **consecutive-duplicate collapsing** and at
most 20 displayed lines. If collapsed output would exceed the bound, the
twentieth line is a truncation notice after 19 displayed output lines. A
quiet wake has no relay; Claude never emits a per-raw-line or last-line
status update.

Scope: this applies **only** to Codex-owned background phases
(`PlanParallelDrafts`, `PlanCodexReviewPending`,
`CodeReviewFixGlobalPending`, `CodeImplementPending+codex`). It does not
change the user-visible idle gap during Claude's Plan Mode prompts, which are
gated on user input.

### Anti-removal: `/ultrareview-local` overlap audit

Under the new v3 ordering, `CodeReviewLocalPending` runs after the copilot's
`review_fix_global`. Its role is **audit of the copilot's commits** plus
catching code-quality, maintainability, consistency, and local-read
issues that the copilot's pr-review-toolkit-backed branch review may miss.
Full-mode `/ultrareview-local` (code-reviewer + security-reviewer +
architect + doc-reviewer in parallel, synthesized by the review worker,
then fixed via isolated fix-subagent fan-out where safe) exercises a
different set of agent prompts, a different synthesis path, and a
different fix integration pass than the copilot's review toolkit turn. Reduced
mode keeps the audit stage but narrows it to no-fix or docs/config-only
cases.

Removing the `CodeReviewLocalPending` stage from the v3 flow therefore
requires a written **overlap audit**: a demonstration, against a
representative sample of prior collab sessions, that the copilot's
`pr-review-toolkit`-backed `review_fix_global` reviews catch the
code-quality issues `/ultrareview-local` would have flagged AND that the
audit-of-the-copilot role is unnecessary (e.g., the copilot's commits never
reintroduce issues).
Without that audit, the stage stays.

**Status as of 2026-07-07: kept with reduced mode.** Code-quality /
consistency overlap with the copilot's branch review is accepted as deliberate.
The written overlap audit above still gates removal. Reduced
`review_local` mode is allowed only after the worker records that the copilot
made no fix commit or that the diff is docs/config-only; uncertainty escalates to
full `/ultrareview-local`.

### Reviewer model selection (out of protocol)

`iron-build` resolves a model per task from the task's `**Tier:**` line, and
runs reviewers at least one tier above the implementer (never below
`standard`). Reviewer model selection is therefore the skill's concern, not
the collab protocol's: collab's own dispatch matrix in
`.claude-plugin/commands/collab.md` pins models for protocol turns only.

**Known divergence on the Codex side.** § Codex model policy above, and
`.codex-plugin/prompts/collab-batch-impl.md` § Model routing, both pin
*implementation workers* flat at `gpt-5.6-luna`/`max`. That predates the tier
system and contradicts it: `max` is not a rung on
`skills/iron-build/references/tiers.md`, and a flat pin collapses the
reviewer floor, because a reviewer one tier above the implementer resolves to
the same model the implementer used. Until that flat pin is scoped to protocol
turns, a `--implementer=codex` batch has two live rules for the same dispatch.
`iron-build`'s tier ladder is the intended winner; do not read the flat pin as
authority over per-task model choice.

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
- v1 planning has one copilot review round; v3 coding is strictly linear (no rounds)
- the dispatcher (always Claude) drives the single planning approval gate at
  `PlanLocked`, after `final` is on the wire and before `task_list` is
  dispatched, regardless of pilot
- the pilot owns the v3 local-audit turn after the copilot's first
  branch-scope review (`CodeReviewLocalPending`) and the final PR-creation
  turn (`CodeReviewFinalPending`)
- Claude runs the dispatcher loop; Codex-owned phases are one-shot
  dispatches that act autonomously and exit

Out of scope:

- multi-session orchestration
- parallel branches / concurrent PRs
- autonomous merge (the Claude submit worker opens the PR under the pilot's
  identity in normal flow; a human merges)
