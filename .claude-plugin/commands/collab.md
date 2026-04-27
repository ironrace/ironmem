---
description: Start or join an IronRace bounded planning session with Codex, auto-flowing into v3 batch coding. Covers v1 planning, v3 batch implementation (Claude or Codex via writing-plans + subagent-driven-development) → global review → PR handoff, and the post-subagent review shortcut. Usage — /collab start [--implementer=claude|codex] <task>  |  /collab join <session_id>  |  /collab review <short-topic>
argument-hint: start [--implementer=claude|codex] <task> | join <session_id> | review <short-topic>
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
   drives every Codex-owned turn via `mcp__codex__codex` in this same
   terminal — there is no second terminal for the user to manage. Just
   report the new `session_id` and selected `implementer` to the user as
   a single line so they can track it:

   ```
   Collab session started: <session_id> (implementer: <claude|codex>)
   ```

   Only fall back to `"Run in Codex: /collab join <session_id>"` if
   `mcp__codex__codex` is not registered (see the Codex handoff section
   below for the fallback path).
5. Enter Plan Mode and draft your first plan for `<task>` — the draft is
   yours alone, Codex cannot see it. When you have the user's approval in
   Plan Mode, call `mcp__ironmem__collab_send` with
   `sender="claude"`, `topic="draft"`, `content=<the plan text>`.
6. After the draft is sent, begin the v1 planning loop (below). When the
   loop observes `current_owner == "codex"`, it drives Codex inline via
   the MCP tool (see "Codex handoff — synchronous MCP invocation"). After
   the plan locks (`PlanLocked`), the session automatically flows into
   the v3 coding bridge (no separate invocation needed).

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
   is Codex's review turn, driven inline via `mcp__codex__codex` under
   the existing "Codex handoff — synchronous MCP invocation" rules.
5. Enter the v3 dispatch loop at phase `CodeReviewFixGlobalPending`. The
   loop handles the two remaining turns (`review_fix_global` from Codex,
   then `final_review` from Claude) and terminates at `CodingComplete`.

## `join <session_id>`

1. Store `<session_id>` as the current collab session — reuse it on every
   subsequent `collab_*` call without re-prompting the user.
2. `agent` / `sender` / `receiver` ← `"claude"` (still Claude's terminal;
   in Codex's terminal this would be `"codex"`, handled by the Codex side).
3. Call `mcp__ironmem__collab_status` to read `task`,
   `phase`, and `current_owner`. Report the task and phase to the user.
4. Branch on the returned `phase`:
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
    Log: t4_phase_advanced_to_<new_phase>   # write timing event

  if session_ended or phase in terminal_set:
    Log: t10_session_complete <phase>       # CodingComplete or CodingFailed
    exit and report to user

  if current_owner == "codex":
    invoke mcp__codex__codex with "/collab join <session_id>"
      (see "Codex handoff — synchronous MCP invocation" below)
    loop  # re-read status when Codex returns

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

## v1 Planning Loop (Phase → Action Table)

Repeat the dispatch loop with these actions:

| Phase | What to do (is_my_turn == true) |
|---|---|
| `PlanParallelDrafts` | Your draft was already sent from the `start` branch. is_my_turn should be false here — if true, verify with `collab_status`. If `collab_status` confirms Claude is the owner in a Codex-owned phase, this is a protocol-level anomaly — exit the loop and report to the user; do not attempt a send. |
| `PlanSynthesisPending` | **Do not ask the user.** Merge both drafts (or revise prior canonical on revision rounds) into a canonical plan. Call `collab_send` with `sender="claude"`, `topic="canonical"`, `content=<plan text>` (plain text — `draft` and `canonical` are the only v1 topics that are NOT JSON-wrapped). |
| `PlanCodexReviewPending` | Codex's turn. is_my_turn should be false — if true, verify with `collab_status`. If the inconsistency persists, exit the loop and report to the user. |
| `PlanClaudeFinalizePending` | **Enter Plan Mode.** Produce the final plan, incorporating Codex's review notes unless they conflict with user intent. Get user approval. Call `collab_send` with `sender="claude"`, `topic="final"`, `content=<JSON string of {"plan":"<full text>"}>` (v1 `final` is the only v1 topic wrapped in JSON). After send, `PlanLocked` is reached. |

Rationale: the user approves only at finalization—the commit point. Everything
before (drafts, synthesis, revisions) runs autonomously.

## v3 Bridge: PlanLocked → CodeImplementPending

Once `PlanLocked` is reached with `final_plan_hash` set and no `task_list`
yet, run the writing-plans + subagent-driven-development pipeline. **Do not
enter harness Plan Mode here** — `superpowers:writing-plans` produces the
markdown plan and presents its own approval handoff to the user, which
serves as the gate.

1. Read `final_plan_hash` and `final_plan` from `collab_status(session_id)`.
   `final_plan` is the JSON string `{"plan":"<full text>"}` Claude
   previously sent; parse it to recover the approved plan body. Read the
   current `HEAD` SHA via `git rev-parse HEAD`.
2. **Invoke `Skill('superpowers:writing-plans')`** with the locked plan
   text as input. The skill will save a markdown plan to
   `docs/superpowers/plans/YYYY-MM-DD-<feature>.md` and present its own
   "execute now?" handoff to the user. This is the user gate. If the user
   declines, abort the bridge cleanly (do not send `task_list`).
3. **Derive the `task_list` manifest from the markdown.** Parse each
   `### Task N:` heading into `{id: N, title: "...", acceptance: [...]}`,
   pulling acceptance criteria from the task body (e.g. lines describing
   what success looks like, or the explicit acceptance bullets if the
   skill includes them). If you parse zero tasks, abort with a clear
   error — do not invent a single-task fallback.
3a. **Detect `mechanical_direct` eligibility.** Before building the
   `task_list` payload, check ALL of:

   1. The writing-plans markdown produced exactly ONE task (`### Task 1`
      only — no `### Task 2` or higher heading is present).
   2. The task's `Files:` block lists one (or zero) files to create or
      modify.
   3. The task's steps include at least one ` ```bash ` block OR a
      ` ```<lang> ` code block with verbatim content meant to be applied
      as-is (not pseudocode or illustrative snippets).
   4. No step contains language like "decide", "choose between",
      "consider alternatives", or other design-judgment cues.

   If ALL four hold, add `"execution_mode": "mechanical_direct"` to the
   `task_list` payload (step 4 below). Otherwise omit the field entirely
   (default subagent-driven behavior).
4. Build `task_list` JSON:
   ```json
   {
     "plan_hash": "<final_plan_hash>",
     "base_sha": "<current HEAD>",
     "head_sha": "<current HEAD>",
     "plan_file_path": "docs/superpowers/plans/YYYY-MM-DD-<feature>.md",
     "execution_mode": "mechanical_direct",
     "tasks": [
       { "id": 1, "title": "...", "acceptance": ["criterion 1"] }
     ]
   }
   ```
   Omit `"execution_mode"` when eligibility (step 3a) is not met — the
   server treats absence as the default subagent-driven path. Do NOT
   send `"execution_mode": "subagent_driven"` as an explicit value; the
   server rejects it as an unknown mode.
5. Call `collab_send(sender="claude", topic="task_list",
   content=<JSON string>)`. Session advances to `CodeImplementPending`.
   The `current_owner` after this transition matches the session's
   `implementer` — which agent runs the batch phase is committed at
   this point. **Log:** `t1_task_list_sent`
6. **Branch on `implementer`** (read it from `collab_status`):

   - **`implementer == "claude"`** — Run the batch locally. Invoke
     `Skill('superpowers:subagent-driven-development')` with the same
     plan file. Auto-proceed through between-task checkpoints — do not
     pause for user approval per task. Each subagent runs TDD, commits,
     and pushes for its own task.

     **Hard stop at the boundary before
     `finishing-a-development-branch`.** That sub-skill prompts the user
     to choose merge/PR/cleanup, which would create a PR outside the
     collab protocol and collide with the `final_review` turn here. Two
     guards apply:

     1. Before invoking `subagent-driven-development`, tell its
        controller-loop the explicit stopping point: "stop after the
        last task is implemented, reviewed, and committed; do *not*
        invoke `superpowers:finishing-a-development-branch`." The skill's
        controller honors that direction.
     2. After the skill returns and before `implementation_done` is
        sent, verify no PR was opened on this branch behind your back:
        `gh pr list --head <branch> --json number --jq 'length'` must
        return `0`. If it returns ≥1, abort with `failure_report` —
        `coding_failure: "skill_overran_pr_boundary: <pr_number>"` —
        because the protocol's invariant has been violated and the
        global-review stage can no longer open the PR cleanly.

     The collab v3 global review flow
     (`review_local` → `review_fix_global` → `final_review` with
     `gh pr create`) is the protocol's canonical PR path.

   - **`implementer == "codex"`** — For `CodeImplementPending`, use the
     background `codex exec` path (see `### Codex batch handoff — background \`codex exec\``).
     **Log:** `t2_codex_dispatched` immediately before launching.
     **Log:** `t3_codex_returned` immediately after the polling loop exits.
     For all other Codex-owned phases, hand off synchronously via
     `mcp__codex__codex`.
     Read `.codex-plugin/prompts/collab.md`, substitute `$ARGUMENTS`
     with `join <session_id>`, and call the MCP tool with that resolved
     prompt and `cwd = repo_path`. Block until Codex returns. Codex
     will read `plan_file_path` from the canonicalized `task_list`,
     run its own `subagent-driven-development` end-to-end (with the
     same `finishing-a-development-branch` carve-out applied on its
     side), and emit `implementation_done` itself before returning.
     Do *not* invoke `superpowers:subagent-driven-development` locally
     in this mode — Codex owns the batch phase.

     **Recovery if `mcp__codex__codex` errors or times out mid-batch.**
     The session is now sitting at `CodeImplementPending` with
     `current_owner == "codex"` and no agent polling — without
     intervention, it never advances. Catch the MCP failure and:

     1. Re-poll `collab_status`. If the phase has already advanced to
        `CodeReviewLocalPending`, Codex managed to emit
        `implementation_done` before the failure surfaced — fall
        through into the global review loop.
     2. Otherwise, decide based on the failure mode:
        - **Transient (timeout, network, server overload, 5xx)**:
          re-dispatch by reading `.codex-plugin/prompts/collab-batch-impl.md`
          (the slim batch-impl prompt) again and calling `mcp__codex__codex`
          once more. Codex will re-enter at `CodeImplementPending`, observe
          the same `task_list` and `plan_file_path`, and resume the batch.
        - **Hard (Codex unregistered, repeated failure, gate
          regression on Codex's side)**: send `failure_report` with
          `sender="claude"`, `topic="failure_report"`,
          `content=<JSON {"coding_failure":"codex_dispatch_failed: <error>"}>`.
          The state machine's branch-drift carve-out admits this from
          a non-owner, transitioning the session to `CodingFailed`.
          Surface the original Codex error to the user.

     If `mcp__codex__codex` is not registered at all, abort with a
     clear error before sending `task_list`: `--implementer=codex`
     requires the Codex MCP server. (The session is still in
     `PlanLocked` at that point, so `collab_end` is valid.)

7. **Subagent failure handling** (Claude-implementer mode only — Codex's
   failures surface inside its own MCP session and Codex emits
   `failure_report` directly per the Codex prompt). If a subagent fails
   mid-batch (irrecoverable bug, persistent test failure, environment
   issue),
   pause, surface the failure to the user, and triage:
   - If retryable, re-dispatch that subagent and continue.
   - If unrecoverable, send `failure_report` with
     `coding_failure: "subagent_failure: <task id>: <concrete reason>"`
     and exit the loop.
8. **On full success in Claude-implementer mode:** run the pre-send
   harness once (fetch, fmt --check, clippy -D warnings), then run
   `cargo test --workspace` as the post-work gate.
   On gate failure, send `failure_report`. On green, send
   `implementation_done` with `{"head_sha":"<current HEAD>"}`. Session
   advances to `CodeReviewLocalPending`. (In Codex-implementer mode
   Codex already emitted `implementation_done` from inside its MCP
   session; just re-poll `collab_status` and confirm the phase is now
   `CodeReviewLocalPending` with Claude as owner.)
9. Fall through into the v3 dispatch loop.

## v3 Dispatch Loop (Phase → Action Table)

v3 batch mode has exactly four Claude-owned coding turns: `task_list`
(bridge), `implementation_done` (post-batch), `review_local` (post-
ultrareview), and `final_review` (PR open). Codex has one coding turn
(`review_fix_global`) at the global review stage. There are no per-task
Codex turns — Claude orchestrates per-task work via subagents on its
side, and Codex's only second-opinion pass is at branch scope.

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
3. **Reset only when Codex just pushed.** Run `git reset --hard <last_head_sha>`
   before `review_local` and `final_review` — Codex pushed `review_fix_global`
   right before those phases. Skip reset before `task_list` and
   `implementation_done` — Claude is the only writer in those phases.
4. Run local gates (pre-work — fmt + clippy only):
   - `cargo fmt --all -- --check`
   - `cargo clippy --workspace --all-targets --all-features -- -D warnings`
   - **No pre-work `cargo test --workspace`.** The receiver just reset to `last_head_sha`, which is the sender-gated commit (every send is post-gated by the sender's harness). Re-running tests on a known-green tree is duplicate work. Branch-drift is already caught at step 2 (`git cat-file -e`). The post-work gate immediately before this turn's `collab_send` runs the full test suite — that's where test execution lives.
5. On any gate failure, send `failure_report` with concrete error message
   (no silent retry). Include the exact error output.
6. Otherwise, proceed to the phase-specific action below.

| Phase | What to do (is_my_turn == true) |
|---|---|
| `CodeImplementPending` | Owner depends on `implementer`. **Claude is owner** (default): the bridge has already invoked `superpowers:subagent-driven-development`. When all subagents finish, **run pre-send harness gates** (no reset — no Codex push to sync) and `collab_send` with `sender="claude"`, `topic="implementation_done"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. Payload carries ONLY `head_sha`. **Codex is owner** (`--implementer=codex`): is_my_turn is false here; the bridge dispatched to Codex via `mcp__codex__codex` and Codex emits `implementation_done` itself before its MCP session returns. If the dispatch loop ever sees `CodeImplementPending` with Codex owner, re-dispatch via the Codex MCP tool (resumed-mid-batch case). |
| `CodeReviewLocalPending` | **Run pre-send harness (with reset to `last_head_sha`), then `/ultrareview-local` on the full task stack.** Fix any CRITICAL/HIGH inline (commit + push). Call `collab_send` with `sender="claude"`, `topic="review_local"`, `content=<JSON {"head_sha":"<current HEAD>"}>`. **Log:** `t5_review_local_sent` |
| `CodeReviewFixGlobalPending` | Codex's turn. is_my_turn should be false. If `collab_status` confirms Claude is the owner, exit the loop and report the anomaly. **Log:** `t6_codex_review_dispatched` immediately before the `mcp__codex__codex` call; **Log:** `t7_codex_review_returned` immediately after it returns. |
| `CodeReviewFinalPending` | **Run pre-send harness (with reset to `last_head_sha`).** Codex has pushed any global fixes (or a no-op commit); your local working copy was reset to `last_head_sha` by the harness. Optionally tweak. Re-run gates. Then **enter Plan Mode**: draft PR title (under 70 chars) and body (summary + test plan derived from task list + gate results). Get user approval. Then `gh pr create --base <base_branch> --head <current branch> --title <approved title> --body <approved body>`. If `gh pr create` fails, send `failure_report` with `coding_failure: "pr_create_failed: <error>"` — no silent retry. On success, **Log:** `t8_pr_created <pr_url>`, capture `pr_url` and call `collab_send` with `sender="claude"`, `topic="final_review"`, `content=<JSON {"head_sha":"<current HEAD>","pr_url":"<https url>"}>`. **Log:** `t9_final_review_sent`. Session advances directly to `CodingComplete`. **Log:** `t10_session_complete CodingComplete`. Exit loop. |

After each send in v3, loop back to polling. The loop continues until
`phase in {CodingComplete, CodingFailed}` or `session_ended`.

**Shortcut entry:** `/collab review` starts the loop at phase
`CodeReviewFixGlobalPending` with `current_owner == "codex"`. No batch
implementation phase is traversed. The only two remaining turns are
Codex's `review_fix_global` and Claude's `final_review`. All
anti-puppeteering rules below apply unchanged.

### Anti-puppeteering rules (v3)

v3 batch mode structurally removes per-task Codex turns and the
`verdict`/`comment` channels that v2 used, but a few behavioral rules
remain:

- The `implementation_done` payload carries ONLY `head_sha`. Do not
  embed subagent notes, self-critique, success summaries, or
  instructions for Codex in any other field — there are no other
  fields. Codex reads the diff and the writing-plans markdown at
  `plan_file_path` to form its own judgment.
- When driving Codex via `mcp__codex__codex`, the `prompt` argument is
  the verbatim expanded Codex prompt with `$ARGUMENTS` substituted —
  nothing more. Use `.codex-plugin/prompts/collab-batch-impl.md` for the
  `CodeImplementPending+codex` turn (slim phase-specific prompt) and
  `.codex-plugin/prompts/collab.md` for all other Codex-owned phases
  (v1 planning, global review). Do not append session context, state
  summary, or recommendations about what Codex should conclude. See the
  handoff section below.
- Codex's `review_fix_global` commit stands as its own judgment. If
  Claude disagrees with a fix during `CodeReviewFinalPending`, the right
  response is to amend the code and commit — not to re-litigate in
  prose.

### Codex MCP tuning matrix

Codex's default reasoning effort is the dominant latency cost on long
silent grinds. The MCP tool accepts a `config` argument that overrides
`CODEX_HOME/config.toml`. Use this matrix to pick the per-phase config
for every Codex handoff. Don't blanket-apply low reasoning — review
and planning turns are where the second-opinion value lives, and a
shallow reviewer defeats the protocol's design.

| Phase from `collab_status` | `implementer` | Config override | Rationale |
|---|---|---|---|
| `CodeImplementPending` | `"codex"` | `{ "model_reasoning_effort": "low" }` | Routed via background `codex exec --reasoning-effort low`, NOT `mcp__codex__codex` — see `### Codex batch handoff — background \`codex exec\`` section. Config shown here for reference only. |
| `CodeReviewFixGlobalPending` | (any) | **none** (defaults preserved) | Reviewer judgment must not be shallow |
| `PlanParallelDrafts` | (any) | **none** | Planning needs reasoning |
| `PlanCodexReviewPending` | (any) | **none** | Plan review needs reasoning |
| `CodeImplementPending` | `"claude"` | n/a — Codex isn't owner | Claude runs subagents on its side; no Codex MCP call |

Match **both** columns when looking up a row: `(any)` is a wildcard,
quoted strings (e.g. `"codex"`, `"claude"`) are exact matches. The two
`CodeImplementPending` rows are distinguished only by `implementer` —
do not stop at the first phase match.

Read `phase` and `implementer` from the `collab_status` you fetched at
the top of the dispatch step (you already do this); branch on them
when constructing the MCP call below.

### Codex handoff — synchronous MCP invocation

> **NOTE:** For `CodeImplementPending` with `implementer == "codex"`, do **NOT**
> use this synchronous MCP path. Use the background-exec path documented in
> [`### Codex batch handoff — background \`codex exec\``](#codex-batch-handoff--background-codex-exec-codeimplementpendingcodex-only)
> instead. This synchronous MCP path is used for all **other** Codex-owned phases:
> `PlanParallelDrafts`, `PlanCodexReviewPending`, and `CodeReviewFixGlobalPending`.

**Whenever `current_owner == "codex"` in a coding-active or planning-active
phase, drive Codex's turn inline via the Codex MCP tool.** Codex CLI
sessions are one-shot and do not sustain `wait_my_turn` loops across
handoffs, so Claude is the single control loop: polling when it's Claude's
turn, spawning Codex via MCP when it's Codex's turn. This applies in three
contexts:

- **After a Claude `collab_send`** that flips the owner to Codex.
- **On `/collab join`** when status returns `current_owner == "codex"`.
- **Inside the dispatch loop** whenever a `collab_status` check shows
  `current_owner == "codex"` — do not fall back to a bare `wait_my_turn`
  poll, because no Codex session is running to change the state.

Procedure:

1. Read a fresh `collab_status`. If `current_owner == "claude"` or
   `phase` is terminal, skip this step and resume polling / exit.
2. If `current_owner == "codex"` and phase is non-terminal, expand the
   Codex slash command locally and pass the resolved prompt to the MCP
   tool. **`codex mcp-server` does not read `.codex-plugin/prompts/` —
   the literal `/collab join <sid>` string would be treated as ordinary
   user text.** So:

   a. Select the prompt file based on phase and implementer:
      - `CodeImplementPending` + `implementer == "codex"` (MCP fallback only —
        preferred path is background `codex exec`): read
        `.codex-plugin/prompts/collab-batch-impl.md` (the slim phase-specific
        prompt — covers only the batch-impl turn so Codex doesn't process
        unreachable v1/review content).
      - All other Codex-owned phases (`PlanParallelDrafts`,
        `PlanCodexReviewPending`, `CodeReviewFixGlobalPending`): read
        `.codex-plugin/prompts/collab.md`.
      Both files live at
      `/Users/jeffreycrum/git-repos/ironrace-memory/.codex-plugin/prompts/`
      — this repo holds the canonical prompts regardless of the target
      `repo_path`.
   b. Substitute `$ARGUMENTS` in the selected file with `join <session_id>`.
   c. Build the `arguments` object:
      - Always include `prompt` (the resolved Codex slash-command text)
        and `cwd` (the session's `repo_path`).
      - Look up the row in the "Codex MCP tuning matrix" above using
        the `phase` and `implementer` you already have from
        `collab_status`. If that row specifies a config override, add
        a `config` field with that exact value. If the row says
        "none", omit `config` entirely.

      Example for the batch-impl row (`CodeImplementPending` +
      `implementer == "codex"`):
      ```json
      {
        "name": "mcp__codex__codex",
        "arguments": {
          "prompt": "<resolved prompt text>",
          "cwd": "<repo_path from collab_status>",
          "config": { "model_reasoning_effort": "low" }
        }
      }
      ```

      Example for any Codex-owned review or planning phase (no batch impl):
      ```json
      {
        "name": "mcp__codex__codex",
        "arguments": {
          "prompt": "<resolved prompt text>",
          "cwd": "<repo_path from collab_status>"
        }
      }
      ```

      Do not pass `model` or any other override — only `config` per the
      matrix. Model swap is intentionally out of scope.

   **The `prompt` argument is the verbatim expanded Codex prompt file
   (selected per step 2a above) with `$ARGUMENTS` substituted —
   nothing more.** Do not append, prepend, or inline any session
   context, state summary, recap of Claude's last message, or
   instructions about what Codex should conclude. Codex reads state
   via its own `collab_status` and `recv` calls and must form its
   own judgment. Hand-crafted prompts that steer Codex toward a
   conclusion ("withdraw objections", "this is pro-forma", "Claude
   intends to fix everything") collapse the review into a
   rubber-stamp and defeat the point of an independent second pass.

   Block on the result. Codex executes its phase-specific action
   (review+fix, global review+fix, etc.) and returns when the session
   flips back to a Claude-owned phase or terminates.
3. When `mcp__codex__codex` returns, resume the dispatch loop at
   `collab_status`. The next iteration will see either a Claude-owned
   phase or a terminal condition.

Applies to every Codex-owned phase: v1 `PlanParallelDrafts` (Codex
drafting), v1 `PlanCodexReviewPending`, v3 `CodeReviewFixGlobalPending`.
(v3 `CodeImplementPending` with `implementer == "codex"` uses the
background-exec path documented below, not this synchronous path.)

If `mcp__codex__codex` is not registered, fall back to the legacy flow:
tell the user to run `/collab join <session_id>` in a Codex terminal,
then `ScheduleWakeup` and resume polling.

### Codex batch handoff — background `codex exec` (CodeImplementPending+codex only)

**When to use this path:** ONLY when ALL three conditions hold:
- `phase == "CodeImplementPending"` AND
- `current_owner == "codex"` AND
- `implementer == "codex"`

In every other case (other Codex-owned phases, or `implementer != "codex"`),
use the synchronous MCP path in "Codex handoff — synchronous MCP invocation".

**Rationale:** `CodeImplementPending` is the longest Codex-owned phase
(full batch implementation). The synchronous `mcp__codex__codex` call gives
no visibility during execution (appears as a hang) and may carry higher
cold-start cost than a direct CLI invocation. Backgrounding it lets Claude
surface real-time progress and detect hangs/failures early.

**Procedure:**

a. Read `.codex-plugin/prompts/collab-batch-impl.md` from the collab repo
   (`/Users/jeffreycrum/git-repos/ironrace-memory/.codex-plugin/prompts/collab-batch-impl.md`
   — the slim phase-specific prompt; covers only the `CodeImplementPending+codex`
   turn so Codex doesn't process unreachable v1/review content).
   Substitute `$ARGUMENTS` with `join <session_id>`. Write the resolved
   prompt to a temp file:
   ```bash
   mkdir -p /tmp/collab-eval && cat > /tmp/codex-prompt-${session_id}.md <<'PROMPT_EOF'
   <resolved prompt text>
   PROMPT_EOF
   ```

b. **Log:** `t2_codex_dispatched`

c. Launch via Bash with `run_in_background: true`:
   ```bash
   cd <repo_path> && codex exec --reasoning-effort low --prompt-file /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1
   ```
   > **CLI note (best-effort, verify with `codex --help`):** The exact flag for
   > a prompt file may be `--prompt-file <path>`, `--file <path>`, or stdin
   > redirect (`< /tmp/codex-prompt-…`). Run `codex exec --help` once at the
   > start of this path to confirm. If stdin is supported, prefer:
   > ```bash
   > cd <repo_path> && codex exec --reasoning-effort low - < /tmp/codex-prompt-${session_id}.md > /tmp/codex-out-${session_id}.log 2>&1
   > ```
   > Document in the log which invocation form was used.

d. **Polling loop** — the dispatcher's interactive surface during this phase.
   Poll every ~10 seconds (via Bash `sleep 10` or `ScheduleWakeup`).

   On each iteration:
   - Call `mcp__ironmem__collab_status(session_id)` to detect phase advance.
     **Log:** `t4_phase_advanced_to_<new_phase>` if phase changed.
   - Read `BashOutput(<bash-id>)` to surface new stdout to the user
     as a one-line update: `[codex bg] <last stdout line>`.

   **Termination conditions** (first match wins):

   1. `collab_status.phase` advances to `CodeReviewLocalPending` →
      Codex emitted `implementation_done` cleanly. **SUCCESS.**
      **Log:** `t3_codex_returned` then continue to step f.

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

e. **Log:** `t3_codex_returned` (if not already logged at condition 1).

f. Resume the normal dispatch loop. The next iteration will see
   `current_owner == "claude"` at `CodeReviewLocalPending`.

**Failure modes:**

- **`codex` not on PATH** → fall back to `mcp__codex__codex` synchronously
  (same resolved prompt, `config: {model_reasoning_effort: "low"}`).
  **Log:** `t2_fallback_to_mcp` in place of `t2_codex_dispatched`.

- **Repository or PATH issues** → capture the error output, send
  `failure_report` with `coding_failure: "codex_exec_env_error: <error>"`.

- **User interrupts (Ctrl+C during polling)** → kill the background Bash
  process via `KillShell`. Do NOT automatically send `failure_report` —
  let the user inspect the session state manually before deciding.

**Compatibility:** Only this one phase is rerouted. All other Codex-owned
phases (`PlanParallelDrafts`, `PlanCodexReviewPending`,
`CodeReviewFixGlobalPending`) continue using `mcp__codex__codex` unchanged.

## Timing instrumentation (eval mode)

Claude writes one timing event per line to `/tmp/collab-eval-${session_id}.log`
at key transition points throughout the dispatcher. This is opt-in and
harmless: worst case a `/tmp` log file is written. Timing events never block
the protocol — if a write fails, swallow the error silently and continue.

**Rationale:** IronRace collab sessions span multiple agents and a long
batch-implementation phase. Post-run shell analysis of the log lets us
reconstruct the latency breakdown (planning vs. Codex dispatch vs. review vs.
PR), measure the background-exec speedup (A.2), and identify hangs.

**Format:** one event per line:
```
<unix_seconds>.<nanos> <event_name> [extra]
```

**Write an event:**
```bash
echo "$(date +%s.%N) <event_name> [extra]" >> /tmp/collab-eval-${session_id}.log
```

**Event list:**

| Event | When to write |
|---|---|
| `t0_session_started` | Right after `collab_start` returns (session_id now known) |
| `t1_task_list_sent` | Right after `collab_send(topic="task_list")` returns |
| `t2_codex_dispatched` | Immediately before launching background `codex exec` (or `mcp__codex__codex` for non-batch phases) |
| `t2_fallback_to_mcp` | When `codex` is not on PATH and falling back to synchronous MCP for the batch phase |
| `t3_codex_returned` | Immediately after the polling loop exits successfully (batch phase) or `mcp__codex__codex` returns (other phases) |
| `t4_phase_advanced_to_<phase>` | Every time a poll observes a new phase (write the new phase name as `[extra]`) |
| `t5_review_local_sent` | After `collab_send(topic="review_local")` returns |
| `t6_codex_review_dispatched` | Immediately before the `mcp__codex__codex` call for `CodeReviewFixGlobalPending` |
| `t7_codex_review_returned` | Immediately after that MCP call returns |
| `t8_pr_created` | After `gh pr create` returns success; include the PR URL as `[extra]` |
| `t9_final_review_sent` | After `collab_send(topic="final_review")` returns |
| `t10_session_complete` | When `collab_status.phase` first reads `CodingComplete` or `CodingFailed`; include the phase as `[extra]` |

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

## Invariants — do not violate

- **Never** call `mcp__ironmem__collab_end` during any active phase. Rejected in:
  - v1 active: `PlanParallelDrafts`, `PlanSynthesisPending`,
    `PlanCodexReviewPending`, `PlanClaudeFinalizePending`.
  - v3 active: `CodeImplementPending`, `CodeReviewLocalPending`,
    `CodeReviewFixGlobalPending`, `CodeReviewFinalPending`.

  Only valid from `PlanLocked` pre-`task_list` (abandon plan), `CodingComplete`,
  or `CodingFailed`.
- **Never** peek at Codex's draft before sending your own during
  `PlanParallelDrafts`. The server enforces blind-draft in `recv`.
- **Harness Plan Mode gates only at: v1 initial `draft` (from `/collab start`),
  v1 `final` (`PlanClaudeFinalizePending`), and v3 `final_review`
  (`CodeReviewFinalPending`, PR creation).** The v3 `task_list` send is
  gated by writing-plans's own approval handoff, not harness Plan Mode.
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

## Unknown subcommand

If `$ARGUMENTS` does not start with `start`, `join`, or `review`, tell the user:

```
Usage: /collab start [--implementer=claude|codex] <task>  |  /collab join <session_id>  |  /collab review <short-topic>
```
