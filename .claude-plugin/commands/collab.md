---
description: Start or join an IronRace bounded planning session with Codex, auto-flowing into v3 batch coding. Covers v1 planning, v3 batch implementation (Claude or Codex via approved task plan + iron-build) → global review → PR handoff, and the post-iron-build review shortcut. Usage — /collab start [--implementer=claude|codex] <task>  |  /collab join [--implementer=claude|codex] <session_id>  |  /collab review <short-topic>
argument-hint: start [--implementer=claude|codex] <task> | join [--implementer=claude|codex] <session_id> | review <short-topic>
---

<!-- DERIVED FROM docs/COLLAB.md — protocol changes must update:
     - docs/COLLAB.md (spec)
     - .claude-plugin/commands/collab.md (this file)
     - .codex-plugin/commands/collab.md (Codex slash command)
     - .codex-plugin/prompts/collab-*.md (Codex phase prompts) -->


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
   - `current_branch` ← output of `git branch --show-current`.
   - **If `current_branch` is non-empty and not `main`/`master`/`trunk`**,
     you're already on an isolated branch (e.g. from a prior `iron-build`
     worktree setup, or the user branched manually before running `/collab start`) — use it
     as-is: `branch` ← `current_branch`, `repo_path` unchanged. Do not create
     another branch or worktree.
   - **Otherwise** (on `main`/`master`/`trunk`, or a detached HEAD), create
     an isolated worktree on a new branch — never record `main`/`master`/
     `trunk` as the collab branch:
     - Derive a slug from `task`: lowercase it, strip everything except
       alphanumerics/spaces/hyphens, collapse whitespace to single hyphens,
       truncate to ~40 chars, trim trailing hyphens (fall back to `session`
       if the result is empty). Candidate branch name: `collab/<slug>`. If a
       branch with that name already exists locally or on `origin`
       (`git show-ref --verify --quiet refs/heads/<name>` /
       `refs/remotes/origin/<name>`), append `-2`, `-3`, … until unique.
     - Pick a worktree directory using **only** the directory-priority list
       in `iron-build`'s *Workspace* section (the numbered list under
       "Choose where the worktree lives", plus its gitignore check), with
       one collab-specific override: where that list's final entry says to
       ask the human for a location, collab defaults to `.worktrees/`
       instead. Collab must never stop to ask — it is running a bounded
       protocol turn, not an interactive session. Nothing else in that
       section applies here: its default-branch consent stop, its dependency
       install, its red-baseline "ask whether to proceed", and its worktree
       creation command are all superseded by this list — step 4 exists
       precisely to escape the default branch, and the creation command is
       the next bullet.
     - `git worktree add "<dir>/<name>" -b "<name>"` (branches from the
       current HEAD).
     - `repo_path` ← the new worktree's absolute path. `branch` ← `<name>`.
     - (**Why a worktree, not just `checkout -b`:** every git operation for
       this session — including Codex's pre-send harness, which does
       `git checkout <branch>; git reset --hard <last_head_sha>` — now runs
       entirely inside the isolated worktree directory, so it can never
       collide with whatever the user's own terminal has checked out.)
     - (**Why never record `main`:** the `branch` field is fixed at
       `collab_start` time with no update API — if it's ever recorded as
       `main`, every later turn that trusts `collab_status.branch` will
       check out and hard-reset local `main`, and the next push lands
       straight on `main`, bypassing PR review entirely.)
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
   Collab session started: <session_id> (implementer: <claude|codex>, branch: <branch>)
   ```

   If step 2 created a new worktree, also report its path on the next line:

   ```
   Working in isolated worktree: <repo_path>
   ```

   Only fall back to `"Run in Codex: /collab join <session_id>"` if
   neither `codex` CLI on PATH nor `mcp__codex__codex` is available
   (see the Codex handoff section below for the fallback path).
5. Draft your first plan for `<task>` autonomously — **no Plan Mode, no
   user approval here.** The draft is yours alone, Codex cannot see it.
   Once drafted, call `mcp__ironmem__collab_send` with
   `sender="claude"`, `topic="draft"`, `content=<the plan text>`. Your
   only planning user gate is the final approved task plan at
   `PlanClaudeFinalizePending`; sending the draft autonomously lets Codex
   start grinding immediately instead of waiting on a user think-time gate.
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

Shortcut entry for post-iron-build flows: skip v1
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
   approved task markdown, and scan the current code/diff to determine
   which acceptance criteria are already complete. If no checkpoint exists,
   fall back to the branch diff plus nearby plan docs under
   `docs/iron/plans/`.
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
   - **`CodingComplete`** → report the status and exit.
   - **`CodingFailed`** → read `coding_failure` and `failed_from_phase` from
     status. If `failed_from_phase` is non-null AND `coding_failure` starts
     with one of the six recoverable prefixes (`git_commit_failed:`,
     `git_push_failed:`, `sandbox_denied:`, `disk_full:`, `network_failed:`,
     `codex_dispatch_failed:`) with detail after the prefix, the session is
     resumable: report the failure to the user and ask whether to resume. On
     confirmation call
     `mcp__ironmem__collab_resume(session_id, agent="claude")`, which restores
     the session to `failed_from_phase` with the resumer as both
     `current_owner` and `recovery_owner`, then enter the v3 dispatch loop at
     that phase via the Recovery override (Pre-send Harness Sequence step 0 —
     resume moves the old `coding_failure` into `pending_failure`, so the
     override fires). If it is not resumable (`collab_resume` returns
     `NotResumable`, or the failure classifies Terminal), report the status
     and exit.

## Dispatch Loop Structure

Both v1 and v3 share a common dispatch loop:

```text
loop:
  status = collab_status(session_id)

  if phase changed since last iteration:
    Log: t4_phase_advanced phase=<new_phase>   # write timing event

  if session_ended or phase in terminal_set:
    Log: t10_session_complete <phase>       # CodingComplete or CodingFailed
    if phase == CodingFailed and resumable:  # see the resumability check below
      offer collab_resume; on confirmation re-enter at failed_from_phase
    else:
      exit and report to user

  if current_owner == "codex":
    dispatch via background `codex exec`
      (see "Codex handoff — background `codex exec`" below)
    loop  # re-read status when Codex's phase advances or returns a compose ref

  # current_owner == "claude"
  recv(session_id, "claude", auto_ack=true)  # atomically acks all returned messages in one round-trip
  # Only fall back to separate collab_ack calls if you need to ack messages selectively.
  act on phase (send exactly one message per iteration)
  loop
```

`wait_my_turn` is only needed to bridge brief race windows where the
server is still writing state after a send. Do NOT use it as a
wait-for-Codex mechanism — Codex isn't polling, Claude drives it.

**Worktree cleanup reminder on `CodingComplete`.** The session's lifecycle
ends here, before a human merges the PR on GitHub — collab has no way to
observe the merge, so it cannot clean up automatically. If `start` created an
isolated worktree for this session (check: `git rev-parse --git-common-dir`
differs from `git rev-parse --git-dir` in `repo_path`), include a line in the
final report to the user naming the worktree path and the cleanup command
(`git worktree remove <path>`, once the PR merges) — do not run cleanup
yourself, since the branch/worktree must survive until the PR is actually
merged.

Terminal sets:
- **v1**: `{PlanLocked}` (until `task_list` is sent)
- **v3**: `{CodingComplete, CodingFailed}`

**`CodingFailed` is only conditionally terminal — the resumability check.**
A session is resumable when `failed_from_phase` is non-null AND
`coding_failure` starts with one of the six recoverable prefixes
(`git_commit_failed:`, `git_push_failed:`, `sandbox_denied:`, `disk_full:`,
`network_failed:`, `codex_dispatch_failed:`) with at least one byte of detail
after the prefix. Anything else — including `branch_drift:` and
`subagent_failure:` — classifies Terminal and is a real exit. For a resumable
session, report the failure and ask the user whether to resume; on
confirmation call `mcp__ironmem__collab_resume(session_id, agent="claude")`
and re-enter the v3 dispatch loop at the restored `failed_from_phase` through
the Recovery override. The server also refuses the third recoverable report
in a session (`MAX_RECOVERY_ATTEMPTS = 2`), degrading it to a real
`CodingFailed`; `collab_resume` then returns `NotResumable`, which is an
exit, not a retry loop.

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
`$BRANCH`, `$TOPIC`, `$ARTIFACT_REF`, `$ARTIFACT_HASH`, `$MODE`, `$SENDER`) → ingests ONLY the worker's
≤3-line verdict → loops. Template path resolution: prefer
`.claude-plugin/prompts/collab-turn-<turn>.md` in the target repo checkout
(present when the target is ironrace-memory itself); otherwise use the
installed copy at `~/.claude/prompts/collab-turn-<turn>.md` (placed by
`scripts/install-ironmem.sh`). If neither exists, stop and report the missing
template — never improvise a worker prompt from memory. The worker calls ironmem MCP tools directly; full
artifacts never transit the orchestrator.

**Anti-puppeteering:** pass ONLY the resolved template. Never append an inline
recap, state summary, or "what to conclude." The worker discovers state via its
own `collab_status` / `collab_recv` / drawer fetches.

**Verdict contract:** the worker's final message is ≤3 lines
(`result:` / `ref:` / `blocker:`). The orchestrator stores only this.

<!-- LINT:gates-ref-only -->
### Approval gates are reference-only
The only planning user gate is `final`: a compose worker writes the
iron-build-compatible task markdown to `docs/iron/plans/...` and stages the
exact markdown in a drawer, then returns `{ref, file path, ≤3-line summary}`.
The orchestrator surfaces ONLY ref+path+summary for approval (never the full
body); `collab-turn-submit.md` sends the approved final artifact by ref. For
drawer refs, **drawer immutability is the integrity anchor** — drawers are
append-only, so an approved `drawer_id`'s content cannot change and no hash
recompute is needed. Post-plan-lock steps run autonomously (no user gate): the
PlanLocked bridge mechanically parses/submits `task_list` from that approved
markdown, and the v3 `final_review` PR creation auto-proceeds (the diff has
already passed `review_fix_global` + `review_local`).

<!-- LINT:fail-closed-tiering -->
### Model tiers + fail-closed
Planning (`draft`, `canonical`, `final`) → `Agent(model=opus)` at
max effort. Review (`review_local`, `final_review`) → `Agent(model=opus)`.
Mechanical (`task_list`, `code-implement` controller, `submit`) →
`Agent(model=sonnet)` / default. Codex uses the explicit phase-based defaults
in the Codex matrix below; it never inherits the caller's personal model
default. "max effort" = the harness thinking-budget
mechanism. **If the harness cannot select the requested tier for a
planning/review dispatch, ABORT the turn and surface to the user — never silently
fall back to a lower tier.**

<!-- LINT:dispatch-matrix -->
### Dispatch matrix
| Phase | Owner | Template | Tier | Model |
|---|---|---|---|---|
| PlanParallelDrafts | orchestrator | `collab-turn-plan-draft.md` | planning | opus |
| PlanSynthesisPending | orchestrator | `collab-turn-plan-synthesis.md` | planning | opus |
| PlanCodexReviewPending (copilot) | orchestrator | `collab-turn-plan-review.md` | review | opus |
| PlanClaudeFinalizePending | orchestrator | `collab-turn-plan-finalize.md` | planning | opus |
| PlanLocked (bridge) | orchestrator | `collab-turn-task-list.md` | mechanical | sonnet |
| CodeImplementPending | orchestrator | `collab-turn-code-implement.md` | mechanical | sonnet |
| CodeReviewFixGlobalPending (copilot) | orchestrator | `collab-turn-review-fix-global.md` | review | opus |
| CodeReviewLocalPending | orchestrator | `collab-turn-review-local.md` | review | opus |
| CodeReviewFinalPending | orchestrator | `collab-turn-final-review.md` | review | opus |
| post-gate send | pilot | `collab-turn-submit.md` | mechanical | sonnet |

## v1 Planning Loop (Phase → Action Table)

Repeat the dispatch loop with these actions:

| Phase | What to do (is_my_turn == true) |
|---|---|
| `PlanParallelDrafts` | The draft worker (`collab-turn-plan-draft.md`, planning/opus) was already dispatched from the `start` branch. is_my_turn should be false here — if true, verify with `collab_status`. If `collab_status` confirms Claude is the owner in a Codex-owned phase, this is a protocol-level anomaly — exit the loop and report to the user; do not attempt a send. |
| `PlanSynthesisPending` | Dispatch `collab-turn-plan-synthesis.md` (planning/opus) autonomously. It merges both blind drafts and sends `topic="canonical"` directly. Do not enter Plan Mode here; the single human planning gate is the final approved task plan. `draft` and `canonical` are the only v1 topics that are NOT JSON-wrapped. Ingest only the ≤3-line verdict; loop. |
| `PlanCodexReviewPending` | Owner depends on `pilot` — the server gates this phase on the **copilot**, not on Codex (`require_actor(actor, copilot(session))` in `crates/ironmem/src/collab/state_machine/mod.rs`), and the phase label is the frozen wire name, not an owner claim. Read `current_owner` from `collab_status`. **`current_owner == "codex"`** (`pilot == "claude"`, the default): Codex's turn. is_my_turn should be false — if true, verify with `collab_status`. If the inconsistency persists, exit the loop and report to the user. **`current_owner == "claude"`** (`pilot == "codex"`, so Claude is the copilot): this is Claude's legitimate turn, not a protocol anomaly — dispatch the matrix worker `collab-turn-plan-review.md` (review/opus), ingest only its ≤3-line verdict, and loop. **Review cap:** the server enforces `MAX_REVIEW_ROUNDS = 1` at `crates/ironmem/src/collab/state_machine/mod.rs:28`. The copilot gets exactly one plan-review pass; after that review the server transitions to `PlanClaudeFinalizePending` regardless of verdict (`approve`, `approve_with_minor_edits`, or `request_changes` all map to the same next phase). Do not model v1 as open-ended iteration or return to synthesis. |
| `PlanClaudeFinalizePending` | **Enter Plan Mode and get user approval — this is the only planning human gate.** Under reference-only gates, dispatch `collab-turn-plan-finalize.md` (planning/opus), which incorporates Codex's one review pass and produces the final iron-build-compatible task markdown in `docs/iron/plans/...`. Every `### Task N:` must be sized for 20 minutes or less and the plan must contain at most 10 tasks. If it would need 11 or more, stop before approval and split the work into independently executable child issues; never merge unrelated work or drop acceptance criteria to evade the limit. The worker stages `{"plan": "<exact markdown>"}` in a drawer and returns `{drawer_id, file path, ≤3-line summary}`; surface ONLY ref+path+summary for approval. Composition is pilot-generic — under `pilot == "claude"` this worker (`collab-turn-plan-finalize.md`) both composes and stages the drawer as just described; under `pilot == "codex"`, Codex composes and stages the equivalent drawer itself via its own `collab-plan-finalize.md` prompt (incorporates the copilot's review notes, saves the plan file, stages `{"plan": "<exact markdown>"}`, and sends nothing) — v1 planning does have a real pilot split here, matching `.codex-plugin/prompts/collab-plan-finalize.md`. Either way, on approval the orchestrator reads `current_owner` from `collab_status` (confirming `final` is the topic authorized for this phase) and dispatches `collab-turn-submit.md` (mechanical/sonnet) with `$TOPIC=final` `$ARTIFACT_REF=<drawer_id>` `$SENDER=<collab_status.current_owner>` to send `topic="final"` (v1 `final` is the only v1 topic wrapped in JSON); drawer immutability is the integrity anchor. Normally `current_owner == pilot` here. The v3 recovery-owner substitution described in the pre-send harness recovery override in step 0 of the **Pre-send Harness Sequence (Claude-owned v3 turns)** and in the `CodeReviewLocalPending`/`CodeReviewFinalPending` recovery row in the **Codex dispatch tuning matrix** does **not** apply to this phase: `pending_failure`/`FailureReport` handling is gated to coding-active phases (`Phase::is_coding_active()`), and `PlanClaudeFinalizePending` is not one of them, so `$SENDER` always resolves to the pilot here — there is no in-flight recovery-owner case to substitute. After send, `PlanLocked` is reached. Ingest only the ≤3-line verdict; loop. |

Rationale: blind drafts, synthesis, and Codex's single review run
autonomously. The final approved task plan is the commit point and the
only artifact worth interrupting the human for.

## v3 Bridge: PlanLocked → CodeImplementPending

<!-- LINT:bridge-worker-owned -->
### v3 bridge (PlanLocked → CodeImplementPending) — worker-owned
The orchestrator does NOT call a separate plan-expansion skill, read a plan
body from status, or build the manifest inline. It dispatches
`collab-turn-task-list.md` once (mechanical/sonnet) with
`$SENDER=<collab_status.current_owner>`. That worker reads the
approved plan's `{plan_file_path, hash}` reference from `collab_status`,
verifies the exact file, parses tasks, and sends `task_list`. Only the worker's
≤3-line verdict crosses the orchestrator boundary.

Once `PlanLocked` is reached with `final_plan_hash` set and no `task_list`
yet, run this worker-owned bridge. **Do not enter harness Plan Mode here** —
the user already approved the final task plan.

1. Read `current_owner` from `collab_status` (confirming `task_list` is the
   topic authorized for this phase) and dispatch `collab-turn-task-list.md`
   (mechanical/sonnet) with `$SENDER=<collab_status.current_owner>`. Normally
   `current_owner == pilot` here: publishing `final` does not reassign
   ownership, so `PlanLocked` is entered owned by the pilot, and under
   `pilot == "codex"` this send must go out as `codex` even though the Claude
   worker does the parse. `PlanLocked` is not a `Phase::is_coding_active()`
   phase, so no recovery-owner substitution applies and `$SENDER` never
   resolves to anything but the current owner read from `collab_status` — it
   must never be hardcoded to `"claude"`. The worker reads
   `final_plan_ref`/`final_plan_hash`, obtains the repo-relative
   `plan_file_path` from that reference, verifies the file's SHA-256 against
   the approved hash, and parses each `### Task N:` heading into
   `{id, title, timebox_minutes, acceptance:[...]}`.
2. The worker must reject the bridge before sending if:
   - there are zero `### Task ` headings
   - there are more than 10 `### Task ` headings (the issue must be split into
     independently executable child issues before collab coding)
   - task IDs are not contiguous `1..N`
   - any task is missing acceptance criteria
   - any task is missing `Timebox: <=20 minutes`
   - any task is sized above 20 minutes
   - the existing plan file's SHA-256 differs from the approved hash
   PlanLocked is pre-coding, so `failure_report` is not valid here; the worker
   returns the concrete issue on `blocker:` and sends nothing.
3. On success, the worker builds the manifest
   `{plan_hash:final_plan_hash, base_sha:<HEAD>, head_sha:<HEAD>,
   plan_file_path:<path>, tasks:[...]}` (adding
   `execution_mode:"mechanical_direct"` only when the single-task eligibility
   rule in `docs/COLLAB.md` holds) and `collab_send`s `topic="task_list"`.
   Ingest only the ≤3-line verdict. Session advances to
   `CodeImplementPending`; the `current_owner` after this transition matches
   the session's current `implementer`. A later
   `/collab join --implementer=...` may reassign `CodeImplementPending`; the
   new owner must resume from the one current logical-keyed ironmem checkpoint plus a fresh
   plan/code scan. **Log:** `t1_task_list_sent`
5a. **Implementation checkpoint rule.** During `CodeImplementPending`,
   the implementer must write durable task-boundary checkpoints via
   `mcp__ironmem__add_drawer`:

   - `wing`: `ironrace-memory`
   - `room`: `collab-checkpoints`
   - `logical_key`: `collab-checkpoint:<session_id>`
   - write `status: started` before each task
   - write `status: completed` after each task is implemented,
     reviewed, committed, and pushed
   - write `status: blocked` before any unrecoverable
     `failure_report`
   - write `status: batch_complete` after final gates pass and before
     `implementation_done`

   Every write replaces the one logical-keyed current drawer for the session;
   carry the complete cumulative `completed_task_ids` list into the replacement
   body so recovery state is never lost.

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
   gates_sha: <HEAD sha that gates ran against|none>
   gates_commands: <exact gate commands separated by " && "|none>
   gates_result: <not_run|passed|failed: short reason>
   summary: <one concise sentence>
   resume_hint: /collab join [--implementer=<claude|codex>] <session_id>
   ```

   On any fresh `/collab join` where `phase == "CodeImplementPending"`
   and `current_owner == "claude"`, fetch the one logical-keyed current drawer
   deterministically with `get_drawer(wing=ironrace-memory,
   room=collab-checkpoints, logical_key=collab-checkpoint:<session_id>)`
   before doing work. If it is absent, search
   `wing=ironrace-memory room=collab-checkpoints` only as a legacy fallback and
   verify the result against git. Use the checkpoint and the git log to resume at
   `next_task_id` (or the `started` task if the
   last checkpoint stopped mid-task), then read the plan and scan the current code/diff to
   verify what is already complete against the acceptance criteria. If the
   current checkpoint is `batch_complete`, first try to reuse its gate
   proof: require clean pushed-head proof, local
   `HEAD == checkpoint.head_sha`, `checkpoint.gates_sha == checkpoint.head_sha`,
   `checkpoint.gates_result` starts with `passed`, and
   `checkpoint.gates_commands` exactly matches the current required gate
   set. When all checks hold, send `implementation_done` without rerunning
   gates. Rerun gates only on HEAD drift, changed gate commands, failed
   pushed-head proof, or a checkpoint that lacks the new gate-proof fields.
   Do not rerun completed tasks.
6. **Branch on `implementer`** (read it from `collab_status`):

   - **`implementer == "claude"`** — Dispatch the matrix worker
     `collab-turn-code-implement.md` (mechanical/sonnet) and ingest its
     ≤3-line verdict. The worker resumes from the one current logical-keyed
     ironmem checkpoint,
     invokes `Skill('iron-build')` on `plan_file_path`
     (auto-proceeding between tasks, writing the step-5a checkpoints before
     and after every task), runs the gates, and `collab_send`s
     `implementation_done` (`{"head_sha":...}`) on green or `failure_report`
     on failure. Two carve-outs the worker enforces internally:

     **Hard stop at the boundary before
     `iron-build`'s *Finishing the Branch* step.** That step prompts the user
     to choose merge/PR/cleanup, which would create a PR outside the
     collab protocol and collide with the `final_review` turn here. Two
     guards apply:

     1. Before invoking `iron-build`, the worker tells its
        controller-loop the explicit stopping point: "stop after the
        last task is implemented, reviewed, and committed; do *not*
        run the *Finishing the Branch* step." The skill's
        controller honors that direction.
     2. After the skill returns and before `implementation_done` is sent, the
        worker verifies the local boundary invariant: the controller reported
        that it stopped at the requested point and the skill output does not
        mention PR creation or the *Finishing the Branch* step. The worker
        does **not** query GitHub by default. It may run
        `gh pr list --head <branch> --json number --jq 'length'` only when the
        controller reports boundary uncertainty or the skill output mentions
        PR creation / the *Finishing the Branch* step; if it returns >=1, abort
        with `failure_report` —
        `coding_failure: "skill_overran_pr_boundary: <pr_number>"`.

     The collab v3 global review flow
     (`review_fix_global` → `review_local` → `final_review` with
     `gh pr create`) is the protocol's canonical PR path.

   - **`implementer == "codex"`** — Use the background `codex exec` path
     for ALL Codex-owned phases (see `### Codex handoff — background \`codex exec\``).
     **Log:** `t2_codex_dispatched` immediately before launching.
     **Log:** `t3_codex_returned` immediately after the settled wait returns.
     For `CodeImplementPending`, Codex will read `plan_file_path` from the
     canonicalized `task_list`, run its own `iron-build`
     end-to-end (with the same *Finishing the Branch* carve-out
     applied on its side), and emit `implementation_done` itself before
     the settled wait wakes on the phase advance.
     Do *not* invoke `iron-build` locally
     in this mode — Codex owns the batch phase.

     **Recovery if `codex exec` errors or times out mid-batch.**
     The session is now sitting at `CodeImplementPending` with
     `current_owner == "codex"` and no active dispatcher wait — without
     intervention, it never advances. Catch the bg-exec failure and:

     1. Read `collab_status` once. If the phase has already advanced to
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
          This is the context-valid off-turn dispatch-failure carve-out: it
          keeps the phase active and assigns recovery to Claude, who must
          complete the interrupted Codex batch through the recovery override.
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
   checkpoint with `gates_sha=<HEAD>`, the exact `gates_commands`, and
   `gates_result=passed`, and send
   `implementation_done` with `{"head_sha":"<current HEAD>"}`. Session
   advances to `CodeReviewFixGlobalPending`. (In Codex-implementer mode
   Codex already emitted `implementation_done` from its own dispatched
   turn; just re-poll `collab_status` and confirm the phase is now
   `CodeReviewFixGlobalPending` with Codex as owner.)
9. Fall through into the v3 dispatch loop.

## v3 Dispatch Loop (Phase → Action Table)

v3 batch mode has four Claude-side/default coding topics: `task_list`
(bridge), `implementation_done` (post-batch in Claude-implementer mode),
`review_local` (post-Codex audit), and `final_review` (PR open). Codex
has one coding review turn (`review_fix_global`) at branch scope, plus
the optional `implementation_done` turn when the session started with
`--implementer=codex`. There are no per-task cross-agent turns — the
selected implementer orchestrates per-task work via subagents on its own
side.

For every Claude-owned coding turn, execute this pre-send harness
sequence before building the payload:

**Pre-send Harness Sequence (Claude-owned v3 turns):**
0. **Recovery override:** if `collab_status.pending_failure` is non-null and
   `current_owner == "claude"`, Claude owns recovery of the interrupted turn
   even if the normal matrix names Codex. Preserve and inspect the existing
   diff; do **not** fetch, checkout, or reset before that inspection. Run the
   interrupted phase's gates, commit + push, then send its normal completion
   event exactly once.
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
   Codex global-review prompt (`.codex-plugin/prompts/collab-global-review.md`);
   the rules in this list
   apply to Claude's send-side harness only.
4. Run local gates for code-changing Claude turns only (pre-work — fmt +
   clippy only):
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - **No pre-work `cargo test --workspace`.** The receiver just reset to `last_head_sha`, which is the sender-gated commit (every send is post-gated by the sender's harness). Re-running tests on a known-green tree is duplicate work. Branch-drift is already caught at step 2 (`git cat-file -e`). The post-work gate immediately before this turn's `collab_send` runs the full test suite — that's where test execution lives.
   - **Skip all local gates when `phase == CodeReviewFinalPending`.** This
     turn does not change code; it performs pushed-head proof in
     `collab-turn-final-review.md` and then opens the PR.
5. On any gate failure, send `failure_report` with concrete error message
   (no silent retry). Include the exact error output.
6. Otherwise, proceed to the phase-specific action below.

| Phase | What to do (is_my_turn == true) |
|---|---|
| `CodeImplementPending` | Owner depends on `implementer`. **Claude is owner** (default or `/collab join --implementer=claude <session_id>`): dispatch the matrix worker `collab-turn-code-implement.md` (mechanical/sonnet) and ingest its ≤3-line verdict; loop. The worker resumes from `ironrace-memory/collab-checkpoints`, scans plan/code state, continues the local `iron-build` batch with the v3-bridge checkpoint rule, runs pre-send harness gates (no reset — no Codex push to sync), writes `status: batch_complete`, and `collab_send`s `sender="claude"`, `topic="implementation_done"`, `content=<JSON {"head_sha":"<current HEAD>"}>` (payload carries ONLY `head_sha`) on green, or `failure_report` on failure. After send, the phase advances to `CodeReviewFixGlobalPending` (Codex's turn — the new v3 order has Codex run `/pr-review-toolkit:review-pr` on the raw post-implementation diff first). **Codex is owner** (`--implementer=codex`): is_my_turn is false here; dispatch Codex via background `codex exec` (per the Codex handoff section). Codex must resume from ironmem checkpoints, scan the plan/code state, and emit `implementation_done` itself before the bg-exec settled wait wakes on the phase advance. |
| `CodeReviewFixGlobalPending` | Owner depends on `pilot`, plus a recovery override — the server gates this phase on the **copilot** (`require_actor_or_recovery(session, actor, copilot(session))` in `crates/ironmem/src/collab/state_machine/mod.rs`), so who owns it follows from `pilot`, not from the phase name. Read `current_owner` from `collab_status`. **Recovery override (checked first):** if `pending_failure` makes Claude the recovery owner, preserve the diff and complete the interrupted turn per the recovery override, sending `review_fix_global`; this is valid delegated completion, not an anomaly. **`current_owner == "codex"`** outside recovery (`pilot == "claude"`, the default): dispatch Codex via background `codex exec`, with the timing logs and `/pr-review-toolkit:review-pr` review pass described in the Codex handoff section. **`current_owner == "claude"`** outside recovery (`pilot == "codex"`, so Claude is the copilot): this is Claude's legitimate turn — dispatch the matrix worker `collab-turn-review-fix-global.md` (review/opus), ingest only its ≤3-line verdict, and loop. Do **not** dispatch Codex here: `collab-global-review.md`'s own ownership guard rejects and exits, which the wait loop reads as a dispatch failure and turns into a spurious `codex_dispatch_failed:` that burns a recovery attempt. After `review_fix_global`, the phase advances to `CodeReviewLocalPending` (Claude's audit turn). |
| `CodeReviewLocalPending` | Dispatch the matrix worker `collab-turn-review-local.md` (review/opus) and ingest its ≤3-line verdict; loop. The worker runs the pre-send harness (with reset to `last_head_sha` — Codex just pushed at `review_fix_global`), then performs the overlap-mode audit. It runs full `/ultrareview-local` when Codex made fix commits or runtime/Rust files changed, and uses `review_local=reduced` when Codex made no fix commit or the branch diff is docs/config-only. Reduced mode is still an audit: inspect the diff summary, changed files, and Codex commits for protocol drift, docs/config breakage, generated metadata inconsistencies, and security-sensitive configuration; escalate to full `/ultrareview-local` on uncertainty or a substantive finding. Confirmed CRITICAL/HIGH/MEDIUM findings are partitioned into temporary worktrees on unique throwaway branches for parallel fix subagents where safe, merged/cherry-picked back, committed + pushed, and `collab_send`s `sender="claude"`, `topic="review_local"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. **Log:** `t5_review_local_sent`. **Anti-removal:** under v3 ordering the stage audits Codex's `review_fix_global` work plus catches issues both agents missed. Its code-quality lens partially overlaps with Codex's `pr-review-toolkit`-backed branch review but does not fully duplicate it. Removing this stage requires a written overlap audit demonstrating that Codex's `review_fix_global` reviews catch the code-quality issues `/ultrareview-local` would have flagged AND that the audit-of-Codex role is unnecessary. |
| `CodeReviewFinalPending` | **Auto-create the PR — no user-approval gate** (the diff already passed `review_fix_global` + `review_local`, and a PR is editable and unmerged after creation; do NOT enter Plan Mode here). Dispatch the matrix worker `collab-turn-final-review.md` (review/opus) with `$MODE=compose`: it performs pushed-head proof only (no reset, no gate rerun) by requiring a clean worktree, `HEAD == last_head_sha`, and local HEAD equal to the pushed upstream/origin branch head, then drafts the PR title (under 70 chars) + body (summary + test plan derived from task list + prior gate evidence / pushed-head proof), writes `{"title":"...","body":"..."}` to a drawer, and returns `{drawer_id, ≤3-line summary}`. If the proof fails, the worker returns a blocker instead of running tests. Composition is pilot-generic — under `pilot == "claude"` this worker (`collab-turn-final-review.md`) composes as just described; under `pilot == "codex"`, Codex composes the equivalent drawer itself via its own `collab-final-review.md` prompt (proves the pushed head, drafts the PR title/body, stages `{"title":"...","body":"..."}`, and sends nothing and opens no PR). Either way, the orchestrator reads `current_owner` from `collab_status` (confirming `final_review` is the topic authorized for this phase) and dispatches `collab-turn-submit.md` (mechanical/sonnet) **directly** with `$TOPIC=final_review` `$ARTIFACT_REF=<drawer_id>` `$SENDER=<collab_status.current_owner>` (drawer immutability is the integrity anchor — the approved drawer's content cannot change, so no hash recompute is needed): it reads the title/body artifact, then runs a plain `gh pr create --base <base_branch> --head <current branch> --title <title> --body <body>` (a **ready** PR — no `--draft`), and on failure sends `failure_report` `coding_failure: "pr_create_failed: <error>"` (no silent retry). Normally `current_owner == pilot` here; under recovery (`collab_status.pending_failure` non-null), `current_owner` may instead be the recovery owner per the recovery override in step 0 of the **Pre-send Harness Sequence (Claude-owned v3 turns)** and the `CodeReviewLocalPending`/`CodeReviewFinalPending` recovery row in the **Codex dispatch tuning matrix** — `CodeReviewFinalPending` is a coding-active phase, so this substitution is live here, and `$SENDER` must always be read from `current_owner`, never assumed to equal `pilot`. On success, **Log:** `t8_pr_created <pr_url>`, the worker captures `pr_url` and `collab_send`s as `$SENDER`, `topic="final_review"`, `content=<JSON {"head_sha":"<current HEAD>","pr_url":"<https url>"}>`. **Log:** `t9_final_review_sent`. Session advances directly to `CodingComplete`. **Log:** `t10_session_complete CodingComplete`. Exit loop. |

After each send in v3, loop back to polling. The loop continues until
`phase in {CodingComplete, CodingFailed}` or `session_ended` — and
`CodingFailed` exits only after the resumability check above clears it as
non-resumable.

**Shortcut entry:** `/collab review` starts the loop at phase
`CodeReviewFixGlobalPending` with `current_owner == "codex"`. No batch
implementation phase is traversed. Codex must recover context by searching
ironmem checkpoints for the branch, reading any referenced plan, and
scanning the current code/diff against that plan before sending
`review_fix_global`. Under the new v3 order the surviving flow is three
turns: Codex's `review_fix_global` (`/pr-review-toolkit:review-pr` plus
parallel fix subagents for confirmed findings on the raw diff) → Claude's
`review_local` (audit Codex's commits via
`/ultrareview-local`, plus parallel fix subagents for confirmed audit
findings) → Claude's `final_review` (PR creation). The
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
  fields. Codex reads the diff and the approved task markdown at
  `plan_file_path` to form its own judgment.
- When dispatching Codex via `codex exec`, the prompt file passed is
  the verbatim expanded Codex prompt with `$ARGUMENTS` substituted —
  nothing more. Use `.codex-plugin/prompts/collab-batch-impl.md` for the
  `CodeImplementPending+codex` turn (slim phase-specific prompt) and
  `.codex-plugin/prompts/collab-plan-draft.md` for `PlanParallelDrafts`,
  `.codex-plugin/prompts/collab-plan-review.md` for `PlanCodexReviewPending`,
  and `.codex-plugin/prompts/collab-global-review.md` for
  `CodeReviewFixGlobalPending`; use `.codex-plugin/prompts/collab-recovery.md`
  only when the recovery override gives Codex `CodeReviewLocalPending` or
  `CodeReviewFinalPending`. Do not append session context, state
  summary, or recommendations about what Codex should conclude. See the
  handoff section below. This rule applies equally when falling back to
  `mcp__codex__codex` — the prompt content must be the verbatim file
  with `$ARGUMENTS` substituted, never hand-crafted steering text.
- Codex's `review_fix_global` integrated fix commit(s) stand as its own judgment. If
  Claude disagrees with a fix during `CodeReviewFinalPending`, the right
  response is to amend the code and commit — not to re-litigate in
  prose.

### Codex dispatch tuning matrix

ALL Codex-owned non-terminal phases now dispatch via background `codex exec`
(not synchronous `mcp__codex__codex`). The matrix below is the repository
default: it governs the prompt file, model, and reasoning effort. Do not let a
caller-supplied Codex default override a row. `gpt-5.6-sol` is reserved for an
explicit architecture/security escalation from within a turn.

| Phase from `collab_status` | Ownership condition | Prompt file | Model | Reasoning effort | Rationale |
|---|---|---|---|---|---|
| `CodeImplementPending` | `"codex"` | `collab-batch-impl.md` | `gpt-5.6-luna` | `max` | Luna is the default implementation controller/worker and handles the batch's design judgment at the higher implementation budget |
| `CodeReviewFixGlobalPending` | `current_owner == "codex"` | `collab-global-review.md` | `gpt-5.6-terra` | `high` | Normal global review gets a dedicated review budget without paying the Sol escalation cost |
| `CodeReviewFixGlobalPending` | `current_owner == "claude"` | n/a — Codex isn't owner | n/a | n/a | The phase is copilot-gated, so under `pilot == "codex"` Claude owns it and runs the Claude worker itself; no Codex dispatch |
| `CodeReviewLocalPending` / `CodeReviewFinalPending` | Codex recovery owner | `collab-recovery.md` | `gpt-5.6-terra` | `high` | Recovery must finish the delegated normal-completion event without restoring the monolithic prompt |
| `PlanParallelDrafts` | (any) | `collab-plan-draft.md` | `gpt-5.6-terra` | `high` | Planning needs an independent draft |
| `PlanSynthesisPending` | Codex normal pilot | `collab-plan-synthesis.md` | `gpt-5.6-terra` | `high` | The pilot synthesizes the two drafts and sends `canonical` |
| `PlanCodexReviewPending` | `current_owner == "codex"` | `collab-plan-review.md` | `gpt-5.6-terra` | `high` | Plan review needs independent judgment |
| `PlanCodexReviewPending` | `current_owner == "claude"` | n/a — Codex isn't owner | n/a | n/a | The phase is copilot-gated (the wire name is frozen, not an owner claim), so under `pilot == "codex"` Claude owns it and runs the Claude worker itself; no Codex dispatch |
| `PlanClaudeFinalizePending` | Codex normal pilot | `collab-plan-finalize.md` | `gpt-5.6-terra` | `high` | The pilot composes and stages the approval artifact without sending |
| `CodeReviewLocalPending` | Codex normal pilot | `collab-review-local.md` | `gpt-5.6-terra` | `high` | The pilot sends the local audit completion |
| `CodeReviewFinalPending` | Codex normal pilot | `collab-final-review.md` | `gpt-5.6-terra` | `high` | The pilot composes and stages the PR artifact without sending |
| `CodeImplementPending` | `"claude"` | n/a — Codex isn't owner | n/a | n/a | Claude runs subagents on its side; no Codex dispatch |

Match both `Phase` and ownership condition when looking up a row. `(any)` is a
wildcard; `Codex normal pilot` means `pilot == "codex"`,
`current_owner == "codex"`, and `pending_failure` is null; `Codex recovery
owner` means the status names Codex as recovery owner. The two
`CodeImplementPending` rows are distinguished only by `implementer`, and the
paired `PlanCodexReviewPending` and `CodeReviewFixGlobalPending` rows only by
`current_owner` (both phases are gated on the copilot, so Claude owns them
under `pilot == "codex"`) — do not stop at the first phase match.

**`-s danger-full-access` applies uniformly across every row above, not
per-row.** The sandbox setting is a property of the protocol, not of the
phase or model a row selects — so it is not a matrix column. Every row that
actually dispatches Codex (i.e. every row except the `implementer=="claude"`
one, which never launches `codex exec` at all) passes the identical
`-s danger-full-access` flag in step (e) of the Codex handoff procedure
below.

Read `phase` and `implementer` from the `collab_status` you fetched at
the top of the dispatch step; branch on them when selecting the prompt
file, model, and reasoning effort below.

**When falling back to `mcp__codex__codex`** (see fallback path in the
handoff section), apply the same prompt file, model, and effort from this
matrix in the `config` object, and set `config.sandbox` to
`"danger-full-access"` to match the CLI launch lines. The matrix's intent is
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

b. Select prompt file, model, and reasoning effort from the "Codex dispatch tuning
   matrix" above using `phase` and `implementer` from `collab_status`:
   - `CodeImplementPending` + `implementer == "codex"` → prompt file:
     `.codex-plugin/prompts/collab-batch-impl.md`; model and reasoning:
     `-m gpt-5.6-luna -c model_reasoning_effort=max`
   - `PlanParallelDrafts` → `.codex-plugin/prompts/collab-plan-draft.md`;
     Codex-normal-pilot `PlanSynthesisPending` →
     `.codex-plugin/prompts/collab-plan-synthesis.md`;
     `PlanCodexReviewPending` → `.codex-plugin/prompts/collab-plan-review.md`;
     Codex-normal-pilot `PlanClaudeFinalizePending` →
     `.codex-plugin/prompts/collab-plan-finalize.md`;
     `CodeReviewFixGlobalPending` →
     `.codex-plugin/prompts/collab-global-review.md`; Codex-normal-pilot
     `CodeReviewLocalPending` → `.codex-plugin/prompts/collab-review-local.md`;
     Codex-normal-pilot `CodeReviewFinalPending` →
     `.codex-plugin/prompts/collab-final-review.md`; recovery-owned
     `CodeReviewLocalPending` / `CodeReviewFinalPending` →
     `.codex-plugin/prompts/collab-recovery.md`; each uses:
     `-m gpt-5.6-terra -c model_reasoning_effort=high`

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
   - For `PlanCodexReviewPending`: **Log:** `t2_codex_dispatched phase=PlanCodexReviewPending round=1`
   - For Codex-normal-pilot `PlanSynthesisPending`, `PlanClaudeFinalizePending`,
     `CodeReviewLocalPending`, or `CodeReviewFinalPending`: **Log:**
     `t2_codex_dispatched phase=<phase> round=1`

e. Launch via Bash with `run_in_background: true`. Pass the model and
   reasoning effort selected above explicitly, plus `-s danger-full-access`
   (required on both launch lines, verbatim — see the rationale below):
   ```bash
   # CodeImplementPending+codex:
   cd <repo_path> && codex exec -m gpt-5.6-luna -c model_reasoning_effort=max -s danger-full-access - < /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1

   # All other Codex-owned phases:
   cd <repo_path> && codex exec -m gpt-5.6-terra -c model_reasoning_effort=high -s danger-full-access - < /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1
   ```
   > The current Codex CLI accepts `-` as the prompt source and reads the
   > resolved prompt from stdin. Keep this stdin form as the canonical launch
   > path; do not use the unsupported `--prompt-file` flag.
   >
   > Both launch lines take the identical sandbox treatment: `-s
   > danger-full-access` is unconditional, never phase-, model-, or
   > topology-dependent, and never omitted.

   **Why Codex runs unsandboxed, by explicit choice:** the flag stays because
   sandboxing demonstrably breaks the protocol. A collab session normally runs
   from a linked worktree, whose `.git` is a file pointing at
   `<main-repo>/.git/worktrees/<name>/`; that per-worktree gitdir and the
   shared object/ref database that Codex's `commit`/`push` turn writes to
   both live outside any workspace-scoped root, so a workspace-write sandbox
   denies `git commit` outright. Denials are also not limited to the
   filesystem: under workspace-write, `cargo test --workspace` failed the
   daemon/doctor tests with "Operation not permitted" because Unix domain
   socket creation was denied, and no set of extra writable roots
   (`--add-dir` or otherwise) can grant that capability. An earlier
   `--add-dir "<common-gitdir>"` workaround addressed only the git-metadata
   half of the problem and is superseded by this flag; do not reintroduce it.

   **What the flag actually costs.** Against the user's own content the
   sandbox buys little: this Codex process is dispatched by the user, on the
   user's own machine, against a repository the agent already holds in full.
   But the boundary `-s danger-full-access` gives up is not agent-vs-user —
   it is agent-vs-**untrusted content**. Codex's `review_fix_global` turn
   runs `/pr-review-toolkit:review-pr` over PR diffs and review comments,
   material a third party can author; prompt-injected instructions there
   execute with full local filesystem and process access. The flag also
   lifts every restriction on **network egress**, so injected content can
   exfiltrate as well as read. **Operational rule:** do not run a collab
   session — and specifically the `review_fix_global` turn — against a branch
   or PR whose diff or review comments come from an untrusted author.

f. **Event-driven wait loop** — the dispatcher's interactive surface during
   this phase. Set `last_phase_change_at` and `last_stdout_at` at dispatch;
   a phase advance or new Codex stdout resets the respective timestamp. The
   600-second hang deadline is measured from
   `max(last_phase_change_at, last_stdout_at)`, so the existing no-progress
   safeguard remains in force without a fixed `collab_status` cadence.

   While this remains a Codex-owned background phase, call
   `mcp__ironmem__collab_wait_my_turn(session_id, "claude", 60)`. Its compact
   response is a union: exactly `{"unchanged": true}` means the 60-second
   wait elapsed with no relevant change before the deadline; any other response
   is a settled full frame from an **actionable post-claim session-state change**.
   Relevant changes include phase, owner, terminal/ended, and
   **recovery-state changes** such as a pending failure or recovery ownership,
   even when Codex remains the owner. This applies only to the Codex-owned
   background phases
   listed above, never to a Claude Plan Mode prompt that is waiting for user
   input.

   Read `BashOutput(<bash-id>)` once per wait wake. If it includes new stdout,
   relay one `[codex bg]` batch for that wake: perform
   **consecutive-duplicate collapsing**, show at most 20 displayed lines, and if the collapsed output
   exceeds that bound replace the twentieth line with `… truncated after 19
   displayed lines`. Do not emit a batch when there is no new stdout. This
   replaces the stale last-line relay and prevents quiet waits from producing
   user-visible chatter.

   - When the wait returns exactly `{"unchanged": true}`, do not call
     `collab_status` and do not produce an idle user update. After the single
     bounded BashOutput drain above, first apply conditions 4, 5, and 6 below
     if the process exited or 600 seconds of no progress elapsed; otherwise
     immediately begin the next wait. Condition 4 (normal compose handoff) is
     checked first among those three: a clean compose exit is a success path,
     not a dispatch failure.
   - On a settled wake, call `mcp__ironmem__collab_status(session_id)` exactly
     once, then apply the existing success, recovery, terminal,
     compose-handoff, process-exit, and hang conditions below in their stated
     order. A changed phase that
     remains Codex-owned is a normal return: select and launch its next
     phase-specific Codex prompt immediately, before considering the previous
     background process's normal exit an error. **Log:**
     `t4_phase_advanced phase=<new_phase> round=<same round as dispatch>` if
     that status reports a phase change.

   **Termination conditions** (first match wins):

   1. `collab_status.phase` advances from the dispatched phase → Codex emitted
      its message cleanly. **SUCCESS.** This is true whether the destination
      owner is Claude or Codex; a Codex-owned destination immediately returns
      to steps b–e to select and launch the next Codex prompt.
      **Log the appropriate return event** with structured metadata
      (same `phase=` / `round=` values used at dispatch in step d):
      - For `CodeImplementPending`: **Log:** `t3_codex_returned phase=CodeImplementPending round=1`
      - For `CodeReviewFixGlobalPending`: **Log:** `t7_codex_review_returned phase=CodeReviewFixGlobalPending round=1`
      - For `PlanParallelDrafts`: **Log:** `t3_codex_returned phase=PlanParallelDrafts round=1`
      - For `PlanCodexReviewPending`: **Log:** `t3_codex_returned phase=PlanCodexReviewPending round=1`
      Continue to step g.

   2. `collab_status.pending_failure` is non-null AND
      `collab_status.current_owner == "claude"` while `phase` is unchanged →
      a recoverable tooling failure was recorded and recovery was handed to
      Claude. **RECOVERY.**
      - Kill the background Bash process via `KillShell` if it is still
        running.
      - Surface `pending_failure` to the user.
      - Exit the wait loop and enter the Pre-send Harness Sequence step 0
        "Recovery override" for the phase named by `recovery_phase`.
      - Do NOT re-dispatch Codex and do NOT abort.

   3. `collab_status.phase` reaches `CodingFailed` →
      a terminal `failure_report` landed (Codex's own, or a tooling report
      that blew the recovery retry ceiling). **ABORT** — surface failure to
      user, exit the dispatcher loop.

   4. `collab_status.phase` is unchanged, `pilot == "codex"`,
      `pending_failure` is null, `current_owner == "codex"`, and a clean
      Codex-pilot compose process exits in `PlanClaudeFinalizePending` or
      `CodeReviewFinalPending` with a non-empty `ref: <drawer_id>` verdict →
      this is a **normal compose
      handoff**, not a dispatch failure. The compose prompts intentionally
      stage a drawer and send nothing, so no phase advance is expected.
      Validate that the verdict has `blocker: none`, retain only its drawer
      ref and short summary, and refresh `collab_status` immediately before
      the submit worker:
      - In `PlanClaudeFinalizePending`, surface only the ref, file path, and
        summary for the existing human approval gate. After approval, confirm
        the phase and owner are still unchanged, then dispatch
        `collab-turn-submit.md` with `$TOPIC=final`,
        `$ARTIFACT_REF=<drawer_id>`, and
        `$SENDER=<collab_status.current_owner>`.
      - In `CodeReviewFinalPending`, dispatch `collab-turn-submit.md`
        directly with `$TOPIC=final_review`, `$ARTIFACT_REF=<drawer_id>`, and
        `$SENDER=<collab_status.current_owner>`.

      The Claude worker physically performs the submit/PR-create operation,
      but `$SENDER` remains the Codex pilot's current protocol identity. Do
      not re-dispatch the compose prompt, and do not emit
      `codex_dispatch_failed:` for this success path. A missing ref, blocker,
      nonzero exit, or changed status remains a real dispatch failure.

   5. Bash background process exits (BashOutput shows "exit code N" or
      process is no longer running) AND no phase advance observed →
      Codex CLI failed silently. **ERROR.**
      - Capture the last 50 lines from `/tmp/codex-out-${session_id}.log`.
      - **Coding-active phases only** (`CodeImplementPending`,
        `CodeReviewFixGlobalPending`, `CodeReviewLocalPending`,
        `CodeReviewFinalPending`): send `collab_send(sender="claude",
          topic="failure_report",
          content=<JSON {"coding_failure":"codex_dispatch_failed: codex exec exited without a phase advance — <last 50 log lines>"}>)`.
        Read `collab_status` again, then exit the wait loop via condition 2
        (recovery handed to Claude) or condition 3 (retry ceiling exceeded).
      - **Planning phases** (`PlanParallelDrafts`, `PlanSynthesisPending`,
        `PlanCodexReviewPending`, `PlanClaudeFinalizePending`): do **not**
        send `failure_report`. The server gates the whole `FailureReport` arm
        on `Phase::is_coding_active()`, so it rejects the report as
        `WrongPhase` and conditions 2 and 3 are unreachable. Kill any surviving
        background process, exit the wait loop, and report the captured log
        tail and the stalled phase to the user as a dispatch failure that
        needs manual intervention.

      `codex_dispatch_failed:` is the ONLY off-turn-admissible prefix Claude
      may use against a Codex-owned turn, and it classifies **Tooling**
      (recoverable) — so conditions 5 and 6 hand recovery to Claude rather
      than aborting the session (condition 4 is a success path and emits no
      `codex_dispatch_failed:` at all). The server admits it only with at least one
      byte of detail after the colon; a bare prefix, or any other prefix
      (`branch_drift:` aside, which is terminal), is rejected off-turn.

   6. Wall time exceeds 600 seconds (configurable) →
      **HANG.**
      - Kill the Bash background process via `KillShell`.
      - **Coding-active phases only:** send `collab_send(sender="claude",
          topic="failure_report",
          content=<JSON {"coding_failure":"codex_dispatch_failed: codex exec exceeded the 600s hang timeout with no phase advance"}>)`.
        Read `collab_status` again, then exit the wait loop via condition 2
        (recovery handed to Claude) or condition 3 (retry ceiling exceeded).
      - **Planning phases:** as in condition 5, `failure_report` is rejected
        outside `Phase::is_coding_active()` — exit the wait loop and report
        the hang and the stalled phase to the user instead.

g. Resume the normal dispatch loop after a settled success or recovery. The
   settled `collab_status` read already observed the Claude-owned or terminal
   condition.

**Failure modes:**

- **`codex` not on PATH** → fall back to `mcp__codex__codex` synchronously
  with the same resolved prompt and the same explicit `config.model` plus
  `config.model_reasoning_effort` selected from the matrix, and
  `config.sandbox: "danger-full-access"` to match the CLI launch lines. For
  example, `CodeImplementPending+codex` uses
  `{model: "gpt-5.6-luna", model_reasoning_effort: "max", sandbox: "danger-full-access"}`
  and normal planning/review uses
  `{model: "gpt-5.6-terra", model_reasoning_effort: "high", sandbox: "danger-full-access"}`. **Log:**
  `t2_fallback_to_mcp` in place of the normal pre-launch event. The fallback
  applies to ALL phases, not just batch impl. If `mcp__codex__codex` is also not
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
  `failure_report` with
  `coding_failure: "codex_dispatch_failed: repository or PATH error — <error>"`.

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
- **Harness Plan Mode gates only at v1 `final`
  (`PlanClaudeFinalizePending`).** The blind `draft` send, canonical synthesis,
  and Codex's one plan-review pass run autonomously. The v3 `task_list` send is
  a mechanical parse/submit from the already-approved task markdown, not
  an extra task-planning handoff and not a harness Plan Mode gate. The v3
  `final_review` PR creation runs autonomously (no gate; the diff already
  passed `review_fix_global` + `review_local`). Every other turn runs
  autonomously.
- **Every v3 `collab_send` payload is JSON** per the matrix in `docs/COLLAB.md`.
  Never send prose payloads for v3 topics.
- **`head_sha` in every v3 payload must be the current `HEAD` AFTER any
  commit/push that preceded this turn.** The server records branch progress
  via `head_sha`.
- **Off-turn failure carve-out:** `branch_drift:` with real detail may be
  reported by either agent. `codex_dispatch_failed:` with real detail may be
  reported off-turn only by Claude while Codex owns the interrupted turn; all
  other reports require `current_owner`. `branch_drift:` is terminal;
  `codex_dispatch_failed:` is recoverable and leaves recovery with the
  counterpart of the interrupted owner.
- If the user interrupts with a question or correction during v1, answer it
  inside the final Plan Mode gate when possible and incorporate it into the
  final approved task plan. During v3, all turns are autonomous — the only
  remaining content gate is v1 `final` (Plan Mode). The bridge mechanically
  submits `task_list` from the approved markdown and `final_review` PR creation
  is gateless.

## Session handoff (fallback succession)

When your context is exhausted mid-session, call `session_handoff` with
`{ session_id, agent: "claude" }` before stopping. The server composes a
deterministic, model-free ` ```ironrace-session-handoff ` block from
persisted state + the one logical-keyed current `collab-checkpoints` drawer — it never asks a
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

## Context-occupancy handoff

The UserPromptSubmit hook injects a one-line notice when context occupancy
crosses a threshold (default 60% warn / 80% handoff, overridable via
`IRONMEM_CONTEXT_WARN_PCT` / `IRONMEM_CONTEXT_HANDOFF_PCT`).

**Automated successor path (autonomous/collab phases):**

1. On a `>= 80%` (Handoff) notice, call `session_handoff(session_id, agent)`
   and capture the **top-level** `handoff_token` and `handoff_block` (the
   token is NOT inside the fenced block).
2. The `task_outcomes.handoffs` counter is incremented automatically inside
   `handle_session_handoff` only when a **fresh** token is issued (gated on
   `!issued.reused`); reusing an existing token does NOT increment it. The
   count reflects handoff **intent** at fresh-issue time, not successor claim.
3. Spawn the successor via background Bash:
   ```
   claude -p "join ironmem collab <sid> with token <handoff_token>"
   ```
   with `run_in_background: true`. The successor's first mutating call
   presents the token and claims the lease (gen+1), making the predecessor
   **inert** (stale-gen rejection). No process coordination required — the
   generation lease is the single writer.
4. End the current turn.

**Cron fallback (where a spawned child cannot outlive its parent):**

When a spawned child cannot outlive its parent, use a one-time local cron
entry as a best-effort fallback (local-only, never committed, self-deletes):
```sh
(crontab -l 2>/dev/null; echo "* * * * * claude -p \"join ironmem collab <sid> with token <token>\" && crontab -l | grep -v 'join ironmem collab <sid>' | crontab -") | crontab -
```
Safety: `<sid>`/`<token>` must be `[A-Za-z0-9_-]` only — ironmem session IDs
are already sanitized to that set, so they are shell-safe here. Never paste a
raw value from an untrusted source into this pipeline.

**Interactive phases (manual flow):**

When the occupancy notice appears in an interactive session:
1. Note the `session_id` in the notice (the Handoff notice includes
   `join collab <sid>` for easy copy).
2. Call `session_handoff(session_id, agent)` to mint the token.
3. Run `/clear` to reset context.
4. Rejoin with `join collab <sid>` (or `join collab <sid> with token <token>`
   if the token was captured).

**Permission allowlist for unattended successor operation:**

An unattended `claude -p` successor needs at minimum:
- `mcp__ironmem__collab_send`, `mcp__ironmem__collab_recv`,
  `mcp__ironmem__collab_ack`, `mcp__ironmem__collab_approve`,
  `mcp__ironmem__collab_set_implementer`,
  `mcp__ironmem__collab_register_caps`,
  `mcp__ironmem__collab_wait_my_turn`, `mcp__ironmem__collab_end`,
  `mcp__ironmem__collab_resume`, `mcp__ironmem__session_handoff`,
  `mcp__ironmem__collab_status`
- `Bash(claude -p "join ironmem collab *":*)` — re-spawn a further successor if
  needed (scope to the join-command form; avoid the broader `Bash(claude -p:*)`)
- Git bash operations as needed for implementation tasks

Configure these in `.claude/settings.json` under `permissions.allow`.

Full semantics: `docs/COLLAB.md` § "Context-occupancy handoff".

## Unknown subcommand

If `$ARGUMENTS` does not start with `start`, `join`, or `review`, tell the user:

```
Usage: /collab start [--implementer=claude|codex] <task>  |  /collab join [--implementer=claude|codex] <session_id>  |  /collab review <short-topic>
```
