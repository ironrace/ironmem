---
description: Start or join an IronRace bounded planning session with Codex, auto-flowing into v3 batch coding. Covers v1 planning, v3 batch implementation (Claude or Codex via writing-plans + subagent-driven-development) → global review → PR handoff, and the post-subagent review shortcut. Usage — /collab start [--implementer=claude|codex] <task>  |  /collab join [--implementer=claude|codex] <session_id>  |  /collab review <short-topic>
argument-hint: start [--implementer=claude|codex] <task> | join [--implementer=claude|codex] <session_id> | review <short-topic>
---

<!-- DERIVED FROM docs/COLLAB.md — protocol changes must update:
     - docs/COLLAB.md (spec)
     - .claude-plugin/commands/collab.md (this file)
     - .codex-plugin/prompts/collab.md (Codex mirror) -->


You are participating in the IronRace bounded collaboration protocol (v1 planning
+ v3 coding). Full spec: `docs/COLLAB.md`. The user has invoked `/collab` with
arguments:

$ARGUMENTS

Parse the first word of `$ARGUMENTS` as the subcommand and behave as follows.

## `start [--implementer=claude|codex] <task>`

Everything except the task is inferred — never ask the user for paths or
branch names.

1. Parse `$ARGUMENTS`:
   - Strip the leading `start` token.
   - Detect optional `--implementer=claude` or `--implementer=codex` flag
     anywhere in the remaining tokens. Default `"claude"` if absent. Reject
     any other value with a usage error (do not silently fall back).
   - `task` ← the remaining text after stripping `start` and the flag.
2. Resolve defaults:
   - `repo_path` ← output of `git rev-parse --show-toplevel` (run via Bash).
   - `branch` ← output of `git branch --show-current`.
   - `initiator` ← `"claude"` (this is Claude's terminal).
3. Call `mcp__ironmem__collab_start` with `repo_path`, `branch`,
   `initiator`, `task`, and `implementer`. The MCP tool returns
   `session_id`, `task`, and the resolved `implementer` — verify it
   matches what you sent. **Log:** `t0_session_started`
4. **Do not ask the user to run anything in a Codex terminal.** Claude
   drives every Codex-owned turn via background `codex exec` in this
   same terminal — there is no second terminal for the user to manage.
   See the "Codex handoff — background `codex exec`" section below for
   the procedure, fallback, and failure modes. Just report the new
   `session_id` and selected `implementer` to the user as a single line
   so they can track it:

   ```
   Collab session started: <session_id> (implementer: <claude|codex>)
   ```

   Only fall back to `"Run in Codex: /collab join <session_id>"` if
   neither `codex` CLI on PATH nor `mcp__codex__codex` is available
   (see the Codex handoff section below for the fallback path).
5. Draft your first plan for `<task>` autonomously — **no Plan Mode, no
   user approval here.** The draft is yours alone, Codex cannot see it.
   Once drafted, call `mcp__ironmem__collab_send` with
   `sender="claude"`, `topic="draft"`, `content=<the plan text>`. Your
   user gate is the combined plan at `PlanSynthesisPending`, not here —
   sending the draft autonomously lets Codex start grinding immediately
   instead of waiting on a user think-time gate.
6. After the draft is sent, begin the v1 planning loop (below). The send
   normally flips `current_owner` to `"codex"`, so the loop's next
   iteration dispatches Codex via bg-exec immediately (see "Codex handoff
   — background `codex exec`") and Codex grinds in parallel while Claude
   polls. **Race exception (fallback only):** if Codex submitted its
   draft first (only possible when the user manually ran `/collab join`
   in a Codex terminal under the fallback path), the phase advances
   directly to `PlanSynthesisPending` with `current_owner == "claude"`
   on the second draft's arrival — the loop will see Claude is owner
   and proceed to synthesis. After the plan locks (`PlanLocked`), the
   session automatically flows into the v3 coding bridge (no separate
   invocation needed).

## `review <short-topic>`

Shortcut entry for post-subagent-driven-development flows: skip v1
planning and v3 batch implementation, and drop straight into the v3
global-review stage with Codex as the reviewer on the already-committed
branch.
Everything except the short topic is inferred — never ask the user for
paths, branches, or SHAs.

1. Resolve defaults:
   - `repo_path` ← output of `git rev-parse --show-toplevel`.
   - `branch` ← output of `git branch --show-current`. If the result is
     empty (detached HEAD) or equals `main`/`master`/`trunk`, abort with
     an error message explaining the shortcut requires a feature branch.
   - `head_sha` ← output of `git rev-parse HEAD`.
   - `base_sha` ← output of `git merge-base origin/main HEAD` (fall back
     to `origin/master` if that fails, then `origin/trunk`). Abort if all
     three fail with a message asking the user to set an upstream.
   - `initiator` ← `"claude"`.
   - `task` ← the remainder of `$ARGUMENTS` after the word `review`.
2. Call `mcp__ironmem__collab_start_code_review` with
   `{repo_path, branch, base_sha, head_sha, initiator, task}`.
3. Report the session id back as a single line:

   ```
   Collab review session started: <session_id>
   ```

4. **Do not enter Plan Mode and do not draft anything.** The shortcut
   positions the session at `CodeReviewFixGlobalPending` — the next action
   is Codex's review turn, driven inline via `codex exec` under the
   existing "Codex handoff — background `codex exec`" rules.
5. Because shortcut sessions have no collab `task_list`, the Codex review
   prompt must recover context before judging the branch: search ironmem
   checkpoints for the same `repo_path`/`branch`, read any referenced
   writing-plans markdown, and scan the current code/diff to determine
   which acceptance criteria are already complete. If no checkpoint exists,
   fall back to the branch diff plus nearby writing-plans docs in the repo.
6. Enter the v3 dispatch loop at phase `CodeReviewFixGlobalPending`. The
   loop handles the three remaining turns (`review_fix_global` from Codex,
   `review_local` from Claude, then `final_review` from Claude) and
   terminates at `CodingComplete`.

## `join [--implementer=claude|codex] <session_id>`

1. Parse `$ARGUMENTS`:
   - Strip the leading `join` token.
   - Detect optional `--implementer=claude` or `--implementer=codex` flag
     anywhere in the remaining tokens. Reject any other value with a usage
     error.
   - `session_id` ← the remaining token. Reject missing or extra tokens.
2. Store `<session_id>` as the current collab session — reuse it on every
   subsequent `collab_*` call without re-prompting the user.
3. `agent` / `sender` / `receiver` ← `"claude"` (still Claude's terminal;
   in Codex's terminal this would be `"codex"`, handled by the Codex side).
4. If the optional implementer flag was present, call
   `mcp__ironmem__collab_set_implementer` with `session_id`,
   `agent="claude"`, and `implementer=<flag value>`, then use the returned
   session record as the current status. This may transfer an active
   `CodeImplementPending` batch to the selected implementer.
5. Otherwise call `mcp__ironmem__collab_status` to read `task`,
   `phase`, `current_owner`, and `implementer`.
6. Report the task, phase, and implementer to the user.
7. Branch on the returned `phase`:
   - **v1 active** (`PlanParallelDrafts` .. `PlanClaudeFinalizePending`) →
     enter the v1 planning loop (see below).
   - **`PlanLocked` pre-task_list** (final_plan_hash set, no task_list yet) →
     enter the v3 bridge (see "v3 bridge" section).
   - **v3 active** (`CodeImplementPending` .. `CodeReviewFinalPending`) →
     enter the v3 dispatch loop at the current phase (see "v3 dispatch loop").
   - **v3 terminal** (`CodingComplete` / `CodingFailed`) →
     report the status and exit.

## Dispatch Loop Structure

Both v1 and v3 share a common dispatch loop:

```text
loop:
  status = collab_status(session_id)

  if phase changed since last iteration:
    Log: t4_phase_advanced phase=<new_phase>   # write timing event

  if session_ended or phase in terminal_set:
    Log: t10_session_complete <phase>       # CodingComplete or CodingFailed
    exit and report to user

  if current_owner == "codex":
    dispatch via background `codex exec`
      (see "Codex handoff — background `codex exec`" below)
    loop  # re-read status when Codex's phase advances

  # current_owner == "claude"
  recv(session_id, "claude", auto_ack=true)  # atomically acks all returned messages in one round-trip
  # Only fall back to separate collab_ack calls if you need to ack messages selectively.
  act on phase (send exactly one message per iteration)
  loop
```

`wait_my_turn` is only needed to bridge brief race windows where the
server is still writing state after a send. Do NOT use it as a
wait-for-Codex mechanism — Codex isn't polling, Claude drives it.

Terminal sets:
- **v1**: `{PlanLocked}` (until `task_list` is sent)
- **v3**: `{CodingComplete, CodingFailed}`

Once `task_list` has been sent, `PlanLocked` is no longer terminal: the session
stays active and the terminal set flips to the v3 set above. The v3 bridge
(below) sends `task_list` and falls directly into the v3 dispatch loop, so the
planning loop never re-polls at `PlanLocked` post-`task_list`.

<!-- LINT:worker-dispatch -->
## Worker-per-turn dispatch (Claude side)

For every Claude-owned protocol turn, the orchestrator: reads slim
`collab_status` → spawns ONE fresh-context worker via the `Agent` tool, prompt =
the verbatim `.claude-plugin/prompts/collab-turn-<turn>.md` with `$VAR`s
substituted (`$SESSION_ID`, and where the template uses them `$REPO_PATH`,
`$BRANCH`, `$TOPIC`, `$ARTIFACT_REF`, `$ARTIFACT_HASH`, `$MODE`) → ingests ONLY the worker's
≤3-line verdict → loops. The worker calls ironmem MCP tools directly; full
artifacts never transit the orchestrator.

**Anti-puppeteering:** pass ONLY the resolved template. Never append an inline
recap, state summary, or "what to conclude." The worker discovers state via its
own `collab_status` / `collab_recv` / drawer fetches.

**Verdict contract:** the worker's final message is ≤3 lines
(`result:` / `ref:` / `blocker:`). The orchestrator stores only this.

<!-- LINT:gates-ref-only -->
### Approval gates are reference-only
The three user gates (first `canonical`, `final`, `final_review`) and the
PlanLocked bridge use a two-phase split: a compose worker writes the artifact to
a drawer/file and returns `{ref, ≤3-line summary}`; the orchestrator
surfaces ONLY ref+summary for approval (never the full body); a
`collab-turn-submit.md` worker sends the approved artifact by ref. For drawer
refs, **drawer immutability is the integrity anchor** — drawers are append-only,
so an approved `drawer_id`'s content cannot change and no hash recompute is
needed.

<!-- LINT:fail-closed-tiering -->
### Model tiers + fail-closed
Planning (`draft`, `canonical`, `final`, `task_list`) → `Agent(model=opus)` at
max effort. Review (`review_local`, `final_review`) → `Agent(model=opus)`.
Mechanical (`code-implement` controller, `submit`) → `Agent(model=sonnet)` /
default. Codex side unchanged (xhigh). "max effort" = the harness thinking-budget
mechanism. **If the harness cannot select the requested tier for a
planning/review dispatch, ABORT the turn and surface to the user — never silently
fall back to a lower tier.**

<!-- LINT:dispatch-matrix -->
### Dispatch matrix
| Phase | Owner | Template | Tier | Model |
|---|---|---|---|---|
| PlanParallelDrafts | claude | `collab-turn-plan-draft.md` | planning | opus |
| PlanSynthesisPending | claude | `collab-turn-plan-synthesis.md` | planning | opus |
| PlanClaudeFinalizePending | claude | `collab-turn-plan-finalize.md` | planning | opus |
| PlanLocked (bridge) | claude | `collab-turn-task-list.md` | planning | opus |
| CodeImplementPending | claude | `collab-turn-code-implement.md` | mechanical | sonnet |
| CodeReviewLocalPending | claude | `collab-turn-review-local.md` | review | opus |
| CodeReviewFinalPending | claude | `collab-turn-final-review.md` | review | opus |
| post-gate send | claude | `collab-turn-submit.md` | mechanical | sonnet |

## v1 Planning Loop (Phase → Action Table)

Repeat the dispatch loop with these actions:

| Phase | What to do (is_my_turn == true) |
|---|---|
| `PlanParallelDrafts` | The draft worker (`collab-turn-plan-draft.md`, planning/opus) was already dispatched from the `start` branch. is_my_turn should be false here — if true, verify with `collab_status`. If `collab_status` confirms Claude is the owner in a Codex-owned phase, this is a protocol-level anomaly — exit the loop and report to the user; do not attempt a send. |
| `PlanSynthesisPending` | Read `review_round` from `collab_status`. **`review_round == 0` (first synthesis): enter Plan Mode and get user approval — this is the user's primary v1 steering gate (the first artifact combining both drafts).** Under reference-only gates, dispatch `collab-turn-plan-synthesis.md` (planning/opus) with `$MODE=compose` to merge both drafts into a drawer and return `{drawer_id, ≤3-line summary}`; surface ONLY ref+summary for approval; on approval dispatch `collab-turn-submit.md` (mechanical/sonnet) with `$TOPIC=canonical` `$ARTIFACT_REF=<drawer_id>` to send it (drawer immutability is the integrity anchor — the approved drawer's content cannot change, so no hash recompute is needed). **`review_round >= 1` (revision rounds, re-entered on `request_changes`): send autonomously, no Plan Mode** — dispatch `collab-turn-plan-synthesis.md` with `$MODE=send`, which revises the prior canonical and sends `topic="canonical"` directly. `draft` and `canonical` are the only v1 topics that are NOT JSON-wrapped; the worker recovers the prior canonical via `collab_status(verbose:true)` reading `canonical_plan`. Ingest only the ≤3-line verdict; loop. |
| `PlanCodexReviewPending` | Codex's turn. is_my_turn should be false — if true, verify with `collab_status`. If the inconsistency persists, exit the loop and report to the user. **Review cap:** the server enforces `MAX_REVIEW_ROUNDS = 2` at `crates/ironmem/src/collab/state_machine/mod.rs:28`. Codex gets at most two review rounds; after the 2nd review the server transitions to `PlanClaudeFinalizePending` regardless of verdict (`approve`, `approve_with_minor_edits`, or `request_changes` all map to the same next phase). Do not model v1 as open-ended iteration. |
| `PlanClaudeFinalizePending` | **Enter Plan Mode and get user approval — this is the v1 commit-point gate.** Under reference-only gates, dispatch `collab-turn-plan-finalize.md` (planning/opus), which produces the final plan (incorporating Codex's review notes unless they conflict with user intent), writes it to a drawer as `{"plan":...}`, and returns `{drawer_id, ≤3-line summary}`; surface ONLY ref+summary for approval. On approval dispatch `collab-turn-submit.md` (mechanical/sonnet) with `$TOPIC=final` `$ARTIFACT_REF=<drawer_id>` to send `topic="final"` (v1 `final` is the only v1 topic wrapped in JSON); drawer immutability is the integrity anchor — the approved drawer's content cannot change, so no hash recompute is needed. After send, `PlanLocked` is reached. Ingest only the ≤3-line verdict; loop. |

Rationale: the user approves the first combined plan and the final plan;
blind draft and revision rounds run autonomously. The first canonical is
the first artifact worth steering (it combines both drafts and gives the
user Codex-side context they lack at draft time); the final is the
commit point.

## v3 Bridge: PlanLocked → CodeImplementPending

<!-- LINT:bridge-worker-owned -->
### v3 bridge (PlanLocked → CodeImplementPending) — worker-owned
The orchestrator does NOT call `Skill('writing-plans')` inline, read verbose
`final_plan`, or build the manifest. It dispatches `collab-turn-task-list.md`
`$MODE=compose` (authors the plan markdown, returns `plan_file_path` + hash),
surfaces path+hash+summary for approval, then dispatches the same template
`$MODE=submit` with `$ARTIFACT_REF=<approved plan_file_path>` and
`$ARTIFACT_HASH=<approved hash>` to recheck the approved file, derive +
validate the manifest, and send `task_list`. Only refs/paths/hashes cross the
orchestrator boundary.

Once `PlanLocked` is reached with `final_plan_hash` set and no `task_list`
yet, run the worker-owned bridge. **Do not enter harness Plan Mode here** —
the `$MODE=compose` worker authors the markdown plan via `writing-plans` in
produce-only mode, then the orchestrator gates on `plan_file_path + hash +
summary`.

1. **Compose.** Dispatch `collab-turn-task-list.md` (planning/opus) with
   `$MODE=compose`. The worker reads `final_plan`/`final_plan_hash` itself
   (via `collab_status(verbose:true)`), invokes `Skill('writing-plans')` in
   produce-only mode, saves
   `docs/superpowers/plans/YYYY-MM-DD-<feature>.md`, and returns the
   `plan_file_path` + content hash in its ≤3-line verdict. Full artifacts
   never transit the orchestrator.
2. **Approve.** Surface ONLY the `plan_file_path` + hash + ≤3-line summary
   for user approval (reference-only gate — never the full plan body). If the
   user declines, abort the bridge cleanly (do not dispatch `$MODE=submit`,
   do not send `task_list`).
3. **Submit.** On approval, dispatch `collab-turn-task-list.md` again with
   `$MODE=submit`, `$ARTIFACT_REF=<approved plan_file_path>`, and
   `$ARTIFACT_HASH=<approved hash>`. The worker recomputes the file's SHA-256
   and aborts with `failure_report` on mismatch, then parses each
   `### Task N:` heading into `{id, title, acceptance:[...]}`, builds the
   `task_list` manifest `{plan_hash, base_sha:<HEAD>, head_sha:<HEAD>,
   plan_file_path:<$ARTIFACT_REF>, tasks:[...]}` (adding
   `execution_mode:"mechanical_direct"` only when the single-task eligibility
   rule in `docs/COLLAB.md` holds), and `collab_send`s `topic="task_list"`.
   If zero tasks parse, the worker sends a
   `failure_report` instead. Ingest only the ≤3-line verdict. Session
   advances to `CodeImplementPending`; the `current_owner` after this
   transition matches the session's current `implementer`. A later
   `/collab join --implementer=...` may reassign `CodeImplementPending`; the
   new owner must resume from the latest ironmem checkpoint plus a fresh
   plan/code scan. **Log:** `t1_task_list_sent`
5a. **Implementation checkpoint rule.** During `CodeImplementPending`,
   the implementer must write durable task-boundary checkpoints via
   `mcp__ironmem__add_drawer`:

   - `wing`: `ironrace-memory`
   - `room`: `collab-checkpoints`
   - write `status: started` before each task
   - write `status: completed` after each task is implemented,
     reviewed, committed, and pushed
   - write `status: blocked` before any unrecoverable
     `failure_report`
   - write `status: batch_complete` after final gates pass and before
     `implementation_done`

   Use this compact content shape:

   ```text
   collab_checkpoint
   session_id: <session_id>
   phase: CodeImplementPending
   implementer: <claude|codex>
   repo_path: <repo_path>
   branch: <branch>
   plan_file_path: <plan_file_path>
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

   On any fresh `/collab join` where `phase == "CodeImplementPending"`
   and `current_owner == "claude"`, search
   `wing=ironrace-memory room=collab-checkpoints` for the `session_id`
   before doing work. Use the newest checkpoint and the git log to resume
   at `next_task_id` (or the `started` task if the last checkpoint
   stopped mid-task), then read the plan and scan the current code/diff to
   verify what is already complete against the acceptance criteria. If the
   newest checkpoint is `batch_complete`, rerun gates and send
   `implementation_done`; do not rerun completed tasks.
6. **Branch on `implementer`** (read it from `collab_status`):

   - **`implementer == "claude"`** — Dispatch the matrix worker
     `collab-turn-code-implement.md` (mechanical/sonnet) and ingest its
     ≤3-line verdict. The worker resumes from the latest ironmem checkpoint,
     invokes `Skill('subagent-driven-development')` on `plan_file_path`
     (auto-proceeding between tasks, writing the step-5a checkpoints before
     and after every task), runs the gates, and `collab_send`s
     `implementation_done` (`{"head_sha":...}`) on green or `failure_report`
     on failure. Two carve-outs the worker enforces internally:

     **Hard stop at the boundary before
     `finishing-a-development-branch`.** That sub-skill prompts the user
     to choose merge/PR/cleanup, which would create a PR outside the
     collab protocol and collide with the `final_review` turn here. Two
     guards apply:

     1. Before invoking `subagent-driven-development`, the worker tells its
        controller-loop the explicit stopping point: "stop after the
        last task is implemented, reviewed, and committed; do *not*
        invoke `finishing-a-development-branch`." The skill's
        controller honors that direction.
     2. After the skill returns and before `implementation_done` is
        sent, the worker verifies no PR was opened on this branch behind its
        back: `gh pr list --head <branch> --json number --jq 'length'` must
        return `0`. If it returns ≥1, abort with `failure_report` —
        `coding_failure: "skill_overran_pr_boundary: <pr_number>"` —
        because the protocol's invariant has been violated and the
        global-review stage can no longer open the PR cleanly.

     The collab v3 global review flow
     (`review_fix_global` → `review_local` → `final_review` with
     `gh pr create`) is the protocol's canonical PR path.

   - **`implementer == "codex"`** — Use the background `codex exec` path
     for ALL Codex-owned phases (see `### Codex handoff — background \`codex exec\``).
     **Log:** `t2_codex_dispatched` immediately before launching.
     **Log:** `t3_codex_returned` immediately after the polling loop exits.
     For `CodeImplementPending`, Codex will read `plan_file_path` from the
     canonicalized `task_list`, run its own `subagent-driven-development`
     end-to-end (with the same `finishing-a-development-branch` carve-out
     applied on its side), and emit `implementation_done` itself before
     the polling loop detects phase advance.
     Do *not* invoke `subagent-driven-development` locally
     in this mode — Codex owns the batch phase.

     **Recovery if `codex exec` errors or times out mid-batch.**
     The session is now sitting at `CodeImplementPending` with
     `current_owner == "codex"` and no agent polling — without
     intervention, it never advances. Catch the bg-exec failure and:

     1. Re-poll `collab_status`. If the phase has already advanced to
        `CodeReviewFixGlobalPending`, Codex managed to emit
        `implementation_done` before the failure surfaced — fall
        through into the global review loop.
     2. Otherwise, decide based on the failure mode:
        - **Transient (timeout, process crash before phase advance)**:
          re-dispatch via `codex exec` once more (use the slim
          `.codex-plugin/prompts/collab-batch-impl.md`). Codex will
          re-enter at `CodeImplementPending`, observe the same
          `task_list` and `plan_file_path`, and resume the batch.
        - **Hard (repeated failure, gate regression on Codex's side)**:
          send `failure_report` with `sender="claude"`,
          `topic="failure_report"`,
          `content=<JSON {"coding_failure":"codex_dispatch_failed: <error>"}>`.
          The state machine's branch-drift carve-out admits this from
          a non-owner, transitioning the session to `CodingFailed`.
          Surface the original Codex error to the user.

     If `codex` is not on PATH, fall back to `mcp__codex__codex` before
     sending `task_list` (see the fallback path in the handoff section).
     If `mcp__codex__codex` is also not registered, abort with a clear
     error: `--implementer=codex` requires either `codex` CLI or the
     Codex MCP server. (The session is still in `PlanLocked` at that
     point, so `collab_end` is valid.)

7. **Subagent failure handling** (Claude-implementer mode only — Codex's
   batch failures surface inside its own Codex turn, whether launched via
   background `codex exec` or the MCP fallback, and Codex emits
   `failure_report` directly per the Codex prompt). If a subagent fails
   mid-batch (irrecoverable bug, persistent test failure, environment
   issue),
   pause, surface the failure to the user, and triage:
   - If retryable, re-dispatch that subagent and continue.
   - If unrecoverable, send `failure_report` with
     `coding_failure: "subagent_failure: <task id>: <concrete reason>"`
     and exit the loop. Before sending the failure report, write a
     `status: blocked` checkpoint for that task.
8. **On full success in Claude-implementer mode:** run the pre-send
   harness once (fetch, fmt --check, clippy -D warnings), then run
   `cargo test --workspace` as the post-work gate.
   On gate failure, write a `status: blocked` checkpoint and send
   `failure_report`. On green, write a `status: batch_complete`
   checkpoint and send
   `implementation_done` with `{"head_sha":"<current HEAD>"}`. Session
   advances to `CodeReviewFixGlobalPending`. (In Codex-implementer mode
   Codex already emitted `implementation_done` from its own dispatched
   turn; just re-poll `collab_status` and confirm the phase is now
   `CodeReviewFixGlobalPending` with Codex as owner.)
9. Fall through into the v3 dispatch loop.

## v3 Dispatch Loop (Phase → Action Table)

v3 batch mode has four Claude-side/default coding topics: `task_list`
(bridge), `implementation_done` (post-batch in Claude-implementer mode),
`review_local` (post-ultrareview), and `final_review` (PR open). Codex
has one coding review turn (`review_fix_global`) at branch scope, plus
the optional `implementation_done` turn when the session started with
`--implementer=codex`. There are no per-task cross-agent turns — the
selected implementer orchestrates per-task work via subagents on its own
side.

For every Claude-owned coding turn, execute this pre-send harness
sequence before building the payload:

**Pre-send Harness Sequence (Claude-owned v3 turns):**
1. `collab_status(session_id)` → read `last_head_sha`.
2. `git fetch` + `git cat-file -e <last_head_sha>^{commit}` — if the commit
   is missing locally after fetch, send `failure_report` with
   `coding_failure: "branch_drift: last_head_sha=<sha> not found in local repo"`
   and exit the loop (do not retry silently). **Skip the `git fetch`** (keep
   the `git cat-file -e` check) before `task_list` and `implementation_done`
   sends — Claude is the only writer in those phases (same condition as
   the reset-skip in step 3), so there's nothing for Codex to have pushed
   that needs syncing. The cat-file check still catches local-tree drift.
3. **Reset only when Codex just pushed (Claude-side rule under new v3 order).**
   Under the new v3 phase order
   (`CodeImplementPending` → `CodeReviewFixGlobalPending` (Codex) →
   `CodeReviewLocalPending` (Claude audit) → `CodeReviewFinalPending` (Claude PR)),
   Codex's only push happens at `review_fix_global`. So:
   - **Reset to `last_head_sha`** before `review_local` (Codex pushed at
     `review_fix_global` — the only Codex push in v3).
   - **Skip reset** before `final_review` (Claude pushed at `review_local`).
   - **Skip reset** before `task_list` and `implementation_done` (Claude is
     the sole writer in those phases).
   Codex's own pre-send harness (sending `review_fix_global`) keeps its
   receive-side fetch/cat-file/checkout/reset-to-`last_head_sha` before
   reviewing — Codex syncs to whatever Claude pushed at `implementation_done`
   so review uses the canonical post-impl head. That rule lives in the
   Codex prompt (`.codex-plugin/prompts/collab.md`); the rules in this list
   apply to Claude's send-side harness only.
4. Run local gates (pre-work — fmt + clippy only):
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - **No pre-work `cargo test --workspace`.** The receiver just reset to `last_head_sha`, which is the sender-gated commit (every send is post-gated by the sender's harness). Re-running tests on a known-green tree is duplicate work. Branch-drift is already caught at step 2 (`git cat-file -e`). The post-work gate immediately before this turn's `collab_send` runs the full test suite — that's where test execution lives.
5. On any gate failure, send `failure_report` with concrete error message
   (no silent retry). Include the exact error output.
6. Otherwise, proceed to the phase-specific action below.

| Phase | What to do (is_my_turn == true) |
|---|---|
| `CodeImplementPending` | Owner depends on `implementer`. **Claude is owner** (default or `/collab join --implementer=claude <session_id>`): dispatch the matrix worker `collab-turn-code-implement.md` (mechanical/sonnet) and ingest its ≤3-line verdict; loop. The worker resumes from `ironrace-memory/collab-checkpoints`, scans plan/code state, continues the local `subagent-driven-development` batch with the v3-bridge checkpoint rule, runs pre-send harness gates (no reset — no Codex push to sync), writes `status: batch_complete`, and `collab_send`s `sender="claude"`, `topic="implementation_done"`, `content=<JSON {"head_sha":"<current HEAD>"}>` (payload carries ONLY `head_sha`) on green, or `failure_report` on failure. After send, the phase advances to `CodeReviewFixGlobalPending` (Codex's turn — the new v3 order has Codex run `/pr-review-toolkit:review-pr` on the raw post-implementation diff first). **Codex is owner** (`--implementer=codex`): is_my_turn is false here; dispatch Codex via background `codex exec` (per the Codex handoff section). Codex must resume from ironmem checkpoints, scan the plan/code state, and emit `implementation_done` itself before the bg-exec polling loop detects phase advance. |
| `CodeReviewFixGlobalPending` | Codex's turn. is_my_turn should be false. If `collab_status` confirms Claude is the owner, exit the loop and report the anomaly. **Log:** `t6_codex_review_dispatched` immediately before launching `codex exec`; Codex's prompt requires `/pr-review-toolkit:review-pr` as the final Codex review pass before this handoff returns to Claude for `/ultrareview-local`. **Log:** `t7_codex_review_returned` immediately after the polling loop exits. Both events take structured `phase=CodeReviewFixGlobalPending round=1` metadata — see step d ("Codex handoff") for the exact form. After Codex sends `review_fix_global` the phase advances to `CodeReviewLocalPending` (Claude's audit turn). |
| `CodeReviewLocalPending` | Dispatch the matrix worker `collab-turn-review-local.md` (review/opus) and ingest its ≤3-line verdict; loop. The worker runs the pre-send harness (with reset to `last_head_sha` — Codex just pushed at `review_fix_global`), then `/ultrareview-local` on the full task stack as audit of Codex's commits, fixes any CRITICAL/HIGH/MEDIUM inline (commit + push), and `collab_send`s `sender="claude"`, `topic="review_local"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. **Log:** `t5_review_local_sent`. **Anti-removal:** under v3 ordering `/ultrareview-local` audits Codex's `review_fix_global` work plus catches code-quality issues both agents missed. Its code-quality lens partially overlaps with Codex's `pr-review-toolkit`-backed branch review but does not fully duplicate it. Removing this stage requires a written overlap audit demonstrating that Codex's `review_fix_global` reviews catch the code-quality issues `/ultrareview-local` would have flagged AND that the audit-of-Codex role is unnecessary. |
| `CodeReviewFinalPending` | **Enter Plan Mode and get user approval — this is the v3 PR-creation gate.** Under reference-only gates, dispatch the matrix worker `collab-turn-final-review.md` (review/opus) with `$MODE=compose`: it runs the pre-send harness (no reset — Claude just pushed at `review_local`), re-runs gates, drafts the PR title (under 70 chars) + body (summary + test plan derived from task list + gate results), writes `{"title":"...","body":"..."}` to a drawer, and returns `{drawer_id, ≤3-line summary}`. Surface ONLY ref+summary for approval. On approval, dispatch `collab-turn-submit.md` (mechanical/sonnet) with `$TOPIC=final_review` `$ARTIFACT_REF=<drawer_id>`: it reads the approved title/body artifact (drawer immutability is the integrity anchor — the approved drawer's content cannot change, so no hash recompute is needed), then runs `gh pr create --base <base_branch> --head <current branch> --title <approved title> --body <approved body>`, and on failure sends `failure_report` `coding_failure: "pr_create_failed: <error>"` (no silent retry). On success, **Log:** `t8_pr_created <pr_url>`, the worker captures `pr_url` and `collab_send`s `sender="claude"`, `topic="final_review"`, `content=<JSON {"head_sha":"<current HEAD>","pr_url":"<https url>"}>`. **Log:** `t9_final_review_sent`. Session advances directly to `CodingComplete`. **Log:** `t10_session_complete CodingComplete`. Exit loop. |

After each send in v3, loop back to polling. The loop continues until
`phase in {CodingComplete, CodingFailed}` or `session_ended`.

**Shortcut entry:** `/collab review` starts the loop at phase
`CodeReviewFixGlobalPending` with `current_owner == "codex"`. No batch
implementation phase is traversed. Codex must recover context by searching
ironmem checkpoints for the branch, reading any referenced plan, and
scanning the current code/diff against that plan before sending
`review_fix_global`. Under the new v3 order the surviving flow is three
turns: Codex's `review_fix_global` (`/pr-review-toolkit:review-pr` plus
confirmed fixes on the raw diff) → Claude's
`review_local` (audit Codex's commits via
`/ultrareview-local`) → Claude's `final_review` (PR creation). The
shortcut-ancestry gate now fires at BOTH `review_fix_global` AND
`review_local` sends when `task_list.is_none()` — each push must
descend from the prior `last_head_sha`. All anti-puppeteering rules
below apply unchanged.

### Anti-puppeteering rules (v3)

v3 batch mode structurally removes per-task Codex turns and the
`verdict`/`comment` channels that v2 used, but a few behavioral rules
remain:

- The `implementation_done` payload carries ONLY `head_sha`. Do not
  embed subagent notes, self-critique, success summaries, or
  instructions for Codex in any other field — there are no other
  fields. Codex reads the diff and the writing-plans markdown at
  `plan_file_path` to form its own judgment.
- When dispatching Codex via `codex exec`, the prompt file passed is
  the verbatim expanded Codex prompt with `$ARGUMENTS` substituted —
  nothing more. Use `.codex-plugin/prompts/collab-batch-impl.md` for the
  `CodeImplementPending+codex` turn (slim phase-specific prompt) and
  `.codex-plugin/prompts/collab.md` for all other Codex-owned phases
  (v1 planning, global review). Do not append session context, state
  summary, or recommendations about what Codex should conclude. See the
  handoff section below. This rule applies equally when falling back to
  `mcp__codex__codex` — the prompt content must be the verbatim file
  with `$ARGUMENTS` substituted, never hand-crafted steering text.
- Codex's `review_fix_global` commit stands as its own judgment. If
  Claude disagrees with a fix during `CodeReviewFinalPending`, the right
  response is to amend the code and commit — not to re-litigate in
  prose.

### Codex dispatch tuning matrix

Codex's default reasoning effort is the dominant latency cost on long
silent grinds. ALL Codex-owned non-terminal phases now dispatch via
background `codex exec` (not synchronous `mcp__codex__codex`). The
matrix below governs HOW `codex exec` is invoked — specifically the
prompt file and the reasoning flag. Don't blanket-apply low reasoning
— review and planning turns are where the independent-judgment value lives,
and a shallow reviewer defeats the protocol's design.

| Phase from `collab_status` | `implementer` | Prompt file | Reasoning flag | Rationale |
|---|---|---|---|---|
| `CodeImplementPending` | `"codex"` | `collab-batch-impl.md` | `-c model_reasoning_effort=xhigh` | RecoverLead-class batch plans carry real design judgment; the T1 livelock showed shallow batch effort can over-gate instead of moving on |
| `CodeReviewFixGlobalPending` | (any) | `collab.md` | *(none — default preserved)* | Reviewer judgment must not be shallow |
| `PlanParallelDrafts` | (any) | `collab.md` | *(none — default preserved)* | Planning needs reasoning |
| `PlanCodexReviewPending` | (any) | `collab.md` | *(none — default preserved)* | Plan review needs reasoning |
| `CodeImplementPending` | `"claude"` | n/a — Codex isn't owner | n/a | Claude runs subagents on its side; no Codex dispatch |

Match **both** `Phase` and `implementer` columns when looking up a row:
`(any)` is a wildcard, quoted strings are exact matches. The two
`CodeImplementPending` rows are distinguished only by `implementer` —
do not stop at the first phase match.

Read `phase` and `implementer` from the `collab_status` you fetched at
the top of the dispatch step; branch on them when selecting the prompt
file and reasoning flag below.

**When falling back to `mcp__codex__codex`** (see fallback path in the
handoff section), apply the same prompt file selection from this
matrix. The `model_reasoning_effort` override for `CodeImplementPending`
becomes a `config` field (`{ "model_reasoning_effort": "xhigh" }`); all
other phases omit `config` (no override). The matrix's intent is
preserved whether the transport is `codex exec` or MCP.

### Codex handoff — background `codex exec`

**ALL Codex-owned non-terminal phases dispatch via this path.** This
covers:
- `PlanParallelDrafts` (Codex draft turn)
- `PlanCodexReviewPending` (Codex plan review)
- `CodeReviewFixGlobalPending` (Codex global review)
- `CodeImplementPending` + `implementer == "codex"` (batch impl)

Codex CLI sessions are one-shot and do not sustain `wait_my_turn` loops
across handoffs, so Claude is the single control loop: polling when it's
Claude's turn, dispatching Codex via `codex exec` when it's Codex's turn.

**Rationale:** The synchronous `mcp__codex__codex` MCP call blocks with
no visibility and carries a cold-start cost that dominated the observed
latency on `PlanCodexReviewPending` (24+ min hang) and
`CodeReviewFixGlobalPending` (171s) in the smoke run on session
`9c3d263a-7452-4c8c-93b9-b05d286df0aa`. Background `codex exec` replaces
the cold-start with a direct CLI fork, surfaces real-time stdout, and
allows hang detection via wall-clock timeout on every Codex-owned phase.

**Procedure:**

a. Read a fresh `collab_status`. If `current_owner == "claude"` or
   `phase` is terminal, skip this step and resume polling / exit.

b. Select prompt file and reasoning flag from the "Codex dispatch tuning
   matrix" above using `phase` and `implementer` from `collab_status`:
   - `CodeImplementPending` + `implementer == "codex"` → prompt file:
     `.codex-plugin/prompts/collab-batch-impl.md`; reasoning flag:
     `-c model_reasoning_effort=xhigh`
   - All other Codex-owned phases (`PlanParallelDrafts`,
     `PlanCodexReviewPending`, `CodeReviewFixGlobalPending`) → prompt file:
     `.codex-plugin/prompts/collab.md`; reasoning flag: *(none — omit)*

   Both files live at
   `/Users/jeffreycrum/git-repos/ironrace-memory/.codex-plugin/prompts/`
   — this repo holds the canonical prompts regardless of the target
   `repo_path`.

c. Substitute `$ARGUMENTS` in the selected file with `join <session_id>`.
   Write the resolved prompt to a temp file:
   ```bash
   mkdir -p /tmp/collab-eval && cat > /tmp/codex-prompt-${session_id}.md <<'PROMPT_EOF'
   <resolved prompt text>
   PROMPT_EOF
   ```

   **Anti-puppeteering:** The resolved prompt is the verbatim file with
   `$ARGUMENTS` substituted — nothing more. Do not append, prepend, or
   inline session context, state summaries, or instructions about what
   Codex should conclude. Codex reads state via its own `collab_status`
   and `recv` calls and must form its own judgment. Hand-crafted steering
   text ("withdraw objections", "this is pro-forma", "Claude intends to
   fix everything") collapses the review into a rubber-stamp and defeats
   the point of an independent second pass.

d. **Log the appropriate timing event** immediately before launch. Use the
   structured-metadata form (`<event_name> phase=<phase> round=<N>`); fill
   `phase=` from `collab_status.phase` and `round=` from
   `collab_status.review_round + 1` for plan/code reviews (or `round=1` for
   batch impl and initial drafts):
   - For `CodeImplementPending`: **Log:** `t2_codex_dispatched phase=CodeImplementPending round=1`
   - For `CodeReviewFixGlobalPending`: **Log:** `t6_codex_review_dispatched phase=CodeReviewFixGlobalPending round=1`
   - For `PlanParallelDrafts`: **Log:** `t2_codex_dispatched phase=PlanParallelDrafts round=1`
   - For `PlanCodexReviewPending`: **Log:** `t2_codex_dispatched phase=PlanCodexReviewPending round=<1 or 2>`

e. Launch via Bash with `run_in_background: true`. Include `-c model_reasoning_effort=xhigh`
   only for `CodeImplementPending+codex`; omit for all other phases:
   ```bash
   # CodeImplementPending+codex:
   cd <repo_path> && codex exec -c model_reasoning_effort=xhigh --prompt-file /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1

   # All other Codex-owned phases:
   cd <repo_path> && codex exec --prompt-file /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1
   ```
   > **CLI note (best-effort, verify with `codex --help`):** The exact flag for
   > a prompt file may be `--prompt-file <path>`, `--file <path>`, or stdin
   > redirect (`< /tmp/codex-prompt-…`). Run `codex exec --help` once at the
   > start of this path to confirm. If stdin is supported, prefer:
   > ```bash
   > cd <repo_path> && codex exec [-c model_reasoning_effort=xhigh] - < /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1
   > ```
   > Document in the log which invocation form was used.

f. **Polling loop** — the dispatcher's interactive surface during this phase.
   Poll on a bounded backoff curve (NOT a fixed cadence — long silent
   grinds churn the dispatcher without producing new information):

   - Start at **10s** poll interval.
   - After **60s** of no progress (no `phase` advance AND no new stdout
     line in `BashOutput`) → escalate to **20s**.
   - After **300s** (5 min) of no progress → escalate to **30s** (cap).
   - **Reset to 10s** on ANY of: phase advance, new stdout line, bg
     process exit, bg process error/signal.
   - 600s hang detection (termination condition 4 below) is unchanged.

   Track "no progress" with two timestamps: `last_phase_change_at` (set
   when `collab_status.phase` differs from the prior poll) and
   `last_stdout_at` (set when `BashOutput` returns at least one new line
   since the prior poll). Time since `max(last_phase_change_at, last_stdout_at)`
   is the "no progress" duration; pick the poll interval from that.

   The backoff applies **only** to Codex-owned background phases
   (`PlanParallelDrafts`, `PlanCodexReviewPending`,
   `CodeReviewFixGlobalPending`, `CodeImplementPending+codex`). It does
   NOT change the user-visible idle gap during Claude's Plan Mode
   prompts — those are gated on user input, not on poll cadence.

   On each iteration:
   - Call `mcp__ironmem__collab_status(session_id)` to detect phase advance.
     **Log:** `t4_phase_advanced phase=<new_phase> round=<same round as dispatch>` if phase changed.
   - Read `BashOutput(<bash-id>)` to surface new stdout to the user
     as a one-line update: `[codex bg] <last stdout line>`.

   **Termination conditions** (first match wins):

   1. `collab_status.phase` advances to a Claude-owned phase →
      Codex emitted its message cleanly. **SUCCESS.**
      **Log the appropriate return event** with structured metadata
      (same `phase=` / `round=` values used at dispatch in step d):
      - For `CodeImplementPending`: **Log:** `t3_codex_returned phase=CodeImplementPending round=1`
      - For `CodeReviewFixGlobalPending`: **Log:** `t7_codex_review_returned phase=CodeReviewFixGlobalPending round=1`
      - For `PlanParallelDrafts`: **Log:** `t3_codex_returned phase=PlanParallelDrafts round=1`
      - For `PlanCodexReviewPending`: **Log:** `t3_codex_returned phase=PlanCodexReviewPending round=<1 or 2>`
      Continue to step g.

   2. `collab_status.phase` reaches `CodingFailed` →
      Codex emitted `failure_report`. **ABORT** — surface failure to user,
      exit the dispatcher loop.

   3. Bash background process exits (BashOutput shows "exit code N" or
      process is no longer running) AND no phase advance observed →
      Codex CLI failed silently. **ERROR.**
      - Capture the last 50 lines from `/tmp/codex-out-${session_id}.log`.
      - Send `collab_send(sender="claude", topic="failure_report",
          content=<JSON {"coding_failure":"codex_exec_failed_silent: <last lines>"}>)`.
      - **ABORT.**

   4. Wall time exceeds 600 seconds (configurable) →
      **HANG.**
      - Kill the Bash background process via `KillShell`.
      - Send `collab_send(sender="claude", topic="failure_report",
          content=<JSON {"coding_failure":"codex_exec_timeout"}>)`.
      - **ABORT.**

   While polling, emit a one-line progress update each iteration
   (`[codex bg] <last stdout line>`) so the user can confirm Codex is alive.

g. Resume the normal dispatch loop. The next `collab_status` poll will
   see a Claude-owned phase or a terminal condition.

**Failure modes:**

- **`codex` not on PATH** → fall back to `mcp__codex__codex` synchronously
  (same resolved prompt; for `CodeImplementPending+codex` add
  `config: {model_reasoning_effort: "xhigh"}`; all other phases omit
  `config`). **Log:** `t2_fallback_to_mcp` in place of the normal pre-launch
  event. The fallback applies to ALL phases, not just batch impl. Do not pass
  `model` or any other override — only `config` per the matrix. Model swap
  is intentionally out of scope. If `mcp__codex__codex` is also not
  registered, tell the user to run `/collab join <session_id>` in a
  Codex terminal, then `ScheduleWakeup` and resume polling. **Never use a
  `/collab` entry command (`/collab start …`, `/collab join …`,
  `/collab review …`) as the `ScheduleWakeup` prompt** — a fired wakeup
  replays the prompt verbatim, which re-invokes the entry command and tries
  to start a duplicate session. The server now rejects that duplicate (see
  the duplicate-session guard invariant below), but you still burn a turn.
  Use a benign diagnostic prompt (e.g. "check collab session `<id>` status
  and resume the dispatch loop") or rely on the background task-completion
  notification instead.

- **Repository or PATH issues** → capture the error output, send
  `failure_report` with `coding_failure: "codex_exec_env_error: <error>"`.

- **User interrupts (Ctrl+C during polling)** → kill the background Bash
  process via `KillShell`. Do NOT automatically send `failure_report` —
  let the user inspect the session state manually before deciding.

## Timing instrumentation (eval mode)

Opt-in latency logging — full event list, format, and post-run analysis
commands live in `docs/COLLAB.md` § "Timing instrumentation (eval mode)".
Writes are best-effort and never block the protocol.

## Invariants — do not violate

- **Never** call `mcp__ironmem__collab_end` during any active phase. Rejected in:
  - v1 active: `PlanParallelDrafts`, `PlanSynthesisPending`,
    `PlanCodexReviewPending`, `PlanClaudeFinalizePending`.
  - v3 active: `CodeImplementPending`, `CodeReviewFixGlobalPending`,
    `CodeReviewLocalPending`, `CodeReviewFinalPending`.

  Only valid from `PlanLocked` pre-`task_list` (abandon plan), `CodingComplete`,
  or `CodingFailed`.
- **Never** peek at Codex's draft before sending your own during
  `PlanParallelDrafts`. The server enforces blind-draft in `recv`.
- **Duplicate-session guard.** `collab_start` / `collab_start_code_review`
  reject a new session when an active one (`ended_at IS NULL`, including a
  session left at `CodingComplete` / `CodingFailed`) already exists for the
  same `repo_path` + `branch`; the error names the existing `session_id`. On
  that error, do **not** retry — resume the named session with
  `/collab join <id>`, or `collab_end` it first if it is genuinely finished.
- **Process attribution guard.** On error `"another active collab session is
  already bound to this MCP process for metrics attribution: <id>"`, do not
  retry blindly — `collab_end` the named session if it is finished, or run the
  new session from a separate server process; stale/ended sessions self-clear.
- **Never `ScheduleWakeup` with a `/collab` entry command as the prompt.** A
  fired wakeup replays it and would re-enter `start`/`join`/`review`. Use a
  benign diagnostic prompt or rely on the background task-completion
  notification; the duplicate-session guard is the server-side backstop, not
  a license to schedule the entry command.
- **Harness Plan Mode gates only at: v1 FIRST `canonical`
  (`PlanSynthesisPending` with `review_round == 0`), v1 `final`
  (`PlanClaudeFinalizePending`), and v3 `final_review`
  (`CodeReviewFinalPending`, PR creation).** The blind `draft` send (from
  `/collab start`) and revision-round canonicals (`PlanSynthesisPending`
  with `review_round >= 1`) run autonomously. The v3 `task_list` send is
  gated by the orchestrator's ref+hash approval of the worker-produced plan,
  not by writing-plans's interactive handoff and not by harness Plan Mode.
  Every other turn runs autonomously.
- **Every v3 `collab_send` payload is JSON** per the matrix in `docs/COLLAB.md`.
  Never send prose payloads for v3 topics.
- **`head_sha` in every v3 payload must be the current `HEAD` AFTER any
  commit/push that preceded this turn.** The server records branch progress
  via `head_sha`.
- **Branch-drift carve-out:** `failure_report` may be sent by either agent at
  any time during a coding-active phase, independent of `current_owner`. It is
  the only topic that bypasses the owner check. A `coding_failure` prefixed
  `"branch_drift:"` is the canonical drift signal; do not suppress it.
- If the user interrupts with a question or correction during v1, answer it
  inside Plan Mode and incorporate it into the next send. During v3,
  gate at writing-plans's approval (bridge) and harness Plan Mode for
  `final_review`; all other turns are autonomous.

## Session handoff (fallback succession)

When your context is exhausted mid-session, call `session_handoff` with
`{ session_id, agent: "claude" }` before stopping. The server composes a
deterministic, model-free ` ```ironrace-session-handoff ` block from
persisted state + the newest `collab-checkpoints` drawer — it never asks a
model to summarize. The response carries both a `handoff_block` (context for
the successor) and a top-level `handoff_token` (the claim credential — not
embedded inside the block).

The successor presents `handoff_token` on its first actor-bearing mutating
call (`collab_send`, `collab_recv`, `collab_ack`, etc.) to **claim** the
generation lease. The claim advances the active generation, making the prior
process **inert** — any subsequent mutating/binding call from the old process
is rejected. Pure reads (`collab_status`, `collab_get_caps`) remain
available to a stale predecessor.

Tokenless first-touch is only allowed at generation 0 (a session that was
never handed off). Once any handoff is claimed (generation > 0) a fresh
process without a token is rejected and must obtain a `session_handoff`
token.

`session_handoff` is a WRITE tool (denied in read-only / restricted MCP
mode) and is itself generation-guarded — a stale caller cannot mint a new
token after a successor has claimed.

Full semantics: `docs/COLLAB.md` § "session_handoff (fallback succession)".

## Unknown subcommand

If `$ARGUMENTS` does not start with `start`, `join`, or `review`, tell the user:

```
Usage: /collab start [--implementer=claude|codex] <task>  |  /collab join [--implementer=claude|codex] <session_id>  |  /collab review <short-topic>
```
