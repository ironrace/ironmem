# Collab `--implementer=codex` Speedup Design

**Date:** 2026-04-26
**Status:** Approved (brainstorm), pending implementation plan
**Smoke test target:** `docs/superpowers/plans/2026-04-25-collab-codex-implementer-smoke-test.md`

## Problem

A single trivial test addition (`merge_top_k` characterization test) driven through the `--implementer=codex` collab pipeline takes 10–15 minutes wall clock. The pain has two components:

1. **Latency** — long silent stretches inside `mcp__codex__codex` calls, dominated by Codex's high-default reasoning effort grinding on tasks that don't need it.
2. **Visibility** — `mcp__codex__codex` is a synchronous, blocking tool call, so Claude cannot surface progress while Codex is working. The slow Codex turn is indistinguishable from a hang.

The user-stated bar is "easy things should be fast." A trivial test addition shouldn't take 10+ minutes regardless of orchestration overhead.

## Goal

Cut the `merge_top_k` smoke test wall time to **≤ 5 minutes end-to-end** with surgical, reversible changes that preserve the protocol's safety properties (gated commits, blind-draft, branch-drift detection, anti-puppeteering).

## Non-goals

- Changing the collab state machine, protocol topics, or invariants.
- Tuning anything outside the `--implementer=codex` pipeline.
- Replacing Codex with a different external agent.
- Live progress streaming for its own sake — only pursued if latency reduction alone leaves the run feeling stuck.

## Architectural reality (constraint)

`mcp__codex__codex` is a synchronous, blocking MCP tool call from Claude's harness. While in flight, Claude cannot run other tools — no log tail, no `BashOutput` poll, no user-facing progress updates. **Real progress visibility requires invoking Codex through a non-blocking path** (background `codex exec` via Bash). That option is preserved as Phase 3, gated on whether the cheaper phases hit the 5-minute budget.

## Layered change plan

Three phases, each gated on smoke-test results from the prior phase. Run the existing `merge_top_k` smoke plan after each phase. Hard cap: **5 min wall clock end-to-end**.

### Phase 0 — Drop redundant receiver-side `cargo test`

**Cheapest, highest-confidence win. Apply unconditionally as a baseline.**

Both `.claude-plugin/commands/collab.md` and `.codex-plugin/prompts/collab.md` define a "Pre-send Harness Sequence" that the *receiving* agent runs at the start of its turn:

1. Read `last_head_sha`.
2. Fetch.
3. Branch-drift check (`git cat-file -e <last_head_sha>^{commit}`).
4. `git reset --hard <last_head_sha>` (when applicable).
5. Run `cargo fmt --check`, `cargo clippy -D warnings`, `cargo test --workspace`.
6. Phase-specific action.

The pre-work `cargo test --workspace` in step 5 is **redundant** under the protocol invariant that every send is gated by the sender's post-work harness:

- Claude runs full gates (fmt + clippy + test) before every Claude-owned send.
- Codex runs final gates before `implementation_done` (in codex-implementer mode) and before `review_fix_global`.
- The receiver immediately resets to `last_head_sha`, which IS the sender-gated commit.

So the receiver re-tests a tree it just established (via reset) is at a sender-validated SHA. The only failure modes the pre-work test could catch are:

- Branch drift (force-push, manual commits) — already detected by step 3 (`git cat-file -e`).
- Local-tree drift after reset — impossible after `git reset --hard` to a clean SHA on a fresh fetch.

Redundant runs in the smoke pipeline:

| Turn | Side | Pre-work `cargo test` today | Decision |
|---|---|---|---|
| `task_list` bridge | Claude | No (bridge has no harness) | unchanged |
| `CodeImplementPending` (codex impl) | Codex | **Already skipped** per existing spec | unchanged |
| `CodeReviewLocalPending` | Claude | Yes | **Drop** |
| `CodeReviewFixGlobalPending` | Codex | Yes | **Drop** |
| `CodeReviewFinalPending` | Claude | Yes | **Drop** |

Three eliminated runs × ~1–3 min each = **3–9 minutes saved** per smoke run, with no safety degradation.

**Files changed (Phase 0):**

- `.claude-plugin/commands/collab.md` — in the "Pre-send Harness Sequence (Claude-owned v3 turns)" subsection, replace step 4 (the `cargo test --workspace` line) with a note that pre-work tests are skipped: receiver only runs `fmt --check` and `clippy -D warnings` after reset; the post-work gate on the sending side already validated the tree.
- `.codex-plugin/prompts/collab.md` — same change in the "Pre-send Harness Sequence (v3 turns only)" subsection. Step 6 ("Run the project's test command... Record failures") is removed for `CodeReviewFixGlobalPending`. The pre-work `cargo test` is already skipped for `CodeImplementPending` per the existing spec.

**What stays (load-bearing):**

- All sender-side post-work gates (fmt + clippy + test) immediately before any send. These are the actual safety belt.
- Pre-work `cargo fmt --check` and `cargo clippy -D warnings`. Cheap (~10s combined), defensive against any local-tree weirdness, and clippy can catch dependency-resolution issues that a fresh `cargo test` shouldn't have to surface.
- Branch-drift detection (`git cat-file -e <last_head_sha>^{commit}` then `failure_report` on miss). Unchanged.

### Phase 1 — Tune `mcp__codex__codex` reasoning effort by phase

Apply on top of Phase 0.

The `mcp__codex__codex` MCP tool accepts a `config` argument that overrides settings from `CODEX_HOME/config.toml`. Today the dispatch loop in `.claude-plugin/commands/collab.md` constructs the call with only `prompt` and `cwd` — no model, no config — so Codex runs at its default reasoning effort, which appears to be the dominant latency cost on a long silent grind.

**Phase-aware tuning matrix** (the dispatch loop already reads `phase` and `implementer` from `collab_status` before each Codex handoff, so it can branch on them):

| Codex-owned phase | Reasoning effort override |
|---|---|
| `CodeImplementPending` (when `implementer == "codex"`) — batch impl, executing an approved plan | `config: { "model_reasoning_effort": "low" }` |
| `CodeReviewFixGlobalPending` — Codex's reviewer judgment, the design's whole second-opinion rationale | **No override** (defaults preserved) |
| `PlanParallelDrafts`, `PlanCodexReviewPending` — v1 planning | **No override** (defaults preserved) |

Critical design choice: **don't blanket-apply low reasoning.** Implementation-following-a-locked-plan is mechanical; review and planning are not. The protocol's whole value is the second opinion at review, and we destroy that by forcing the reviewer to think shallowly.

**Files changed (Phase 1):**

- `.claude-plugin/commands/collab.md`, in the "Codex handoff — synchronous MCP invocation" subsection: extend the documented call shape from `{prompt, cwd}` to optionally include `config` based on the phase-tuning matrix above. Add a small "Codex MCP tuning matrix" subsection that codifies the table.

**Files explicitly NOT changed (Phase 1):**

- `model` is left at default. Model swap is a separate quality axis (a faster model may regress correctness even on simple tasks). Reasoning effort is the cheaper, more reversible first lever. If Phase 1 lands the budget, we never need to consider model swap.
- `.codex-plugin/prompts/collab.md` — no Codex-side changes for Phase 1. The override is delivered by Claude in the MCP arguments.
- The protocol or state machine.

### Phase 2 — Reserved for further audit (gated)

Only enter Phase 2 if Phase 0 + Phase 1 don't hit the 5-minute budget.

Candidates if it comes to that:

- Trim the resolved Codex prompt itself for the batch-impl turn (it currently includes v1, v3-review, and shortcut-entry sections that aren't reachable from `CodeImplementPending`).
- Skip `git fetch` on the sender side immediately before send-time gates (the receiver fetches; the sender just pushed locally).
- Other duplications discovered in the smoke run trace.

We design Phase 2 in detail only if data justifies it.

### Phase 3 — Background `codex exec` for visibility (gated)

Only enter Phase 3 if Phases 0+1(+2) hit the wall-time budget but the runs still feel stuck (long silent stretches that are hard to distinguish from a hang).

**Outline only:**

Replace the synchronous `mcp__codex__codex` call for `CodeImplementPending` (codex implementer only) with a backgrounded `codex exec --reasoning-effort low <resolved-prompt>` invocation via Bash `run_in_background: true`. Claude alternates `BashOutput` polls (surfacing new stdout to the user) with `collab_status` polls (detecting `implementation_done`). Codex inherits its MCP server config from `~/.codex/config.toml` so `ironmem_collab_*` calls work identically.

Failure modes to design for at that point: codex CLI not on PATH, MCP config drift, stdout buffering / encoding, stale background process on session abort. The other Codex turns (`review_fix_global`, v1 planning) keep using `mcp__codex__codex` since they're shorter and the visibility ROI is lower.

Full Phase 3 design happens after measurement justifies the complexity.

## Test plan & decision rules

**Test target (unchanged):** `docs/superpowers/plans/2026-04-25-collab-codex-implementer-smoke-test.md` — the `merge_top_k` characterization test. End-to-end measurable from `mcp__ironmem__collab_start` to `CodingComplete`.

**Procedure:**

1. Apply Phase 0 + Phase 1 together (both cheap, surgical).
2. Start a fresh smoke session: `/collab start --implementer=codex <merge_top_k task description>`.
3. Stopwatch wall time end-to-end.
4. **Hard cap: 5 minutes.** Watch the clock.

**Decision rules:**

- `T ≤ 5 min` AND PR opened with the expected one-line diff AND no `failure_report` AND `gh pr list --head feat/collab-batch-implementation --json number --jq 'length'` returns `1` → **done**. Commit the changes, write the result, stop.
- `T > 5 min` OR Codex emits `failure_report` from low-reasoning sloppiness OR PR contains unexpected diff → **kill and escalate to Phase 2**.

**Killing a stuck run:**

- Abort the in-flight `mcp__codex__codex` call (Ctrl+C from user terminal).
- Send `failure_report` from Claude's side with `coding_failure: "manual_kill: latency_budget_exceeded"`. The protocol's branch-drift carve-out admits `failure_report` from the non-owner. Session moves to `CodingFailed` cleanly.
- Start a fresh smoke session for the next phase.

**Project-level guardrail:** if Phase 3 also misses the 5-min budget, the path itself isn't viable — surface that to the user and reopen the design conversation rather than escalating into uncharted phases.

## Risks & rollback

| Phase | Risk | Mitigation | Rollback |
|---|---|---|---|
| 0 | Pre-work test was actually catching something the post-work gate missed | Branch-drift detection (`git cat-file -e`) + sender's post-work gate together cover the same failure modes; no realistic gap | Revert the prompt-file edits |
| 1 | Low-reasoning Codex produces broken code on impl turn | Post-work gates (fmt + clippy + test) on Codex's `implementation_done` send catch correctness regressions; Claude's local-review and Codex's global-review further filter | Remove the `config` override for `CodeImplementPending` |
| 1 | Reviewer quality degrades because override leaks | Matrix is phase-keyed; review phase explicitly has no override; covered by code review of the prompt-file change | Same as above |
| 2 / 3 | Designed only on demand | n/a (gated) | n/a |

All Phase 0 and Phase 1 changes are confined to two prompt files. No Rust code, schema, or protocol changes. Reversible by `git revert` of the implementing commits.

## Out of scope

- Adding timing instrumentation to the collab DB (`created_at` / `completed_at` on sessions and phase transitions). Useful for future tuning, but a separate concern; we baseline by stopwatch this round.
- Tuning the Claude-implementer path (`/collab start <task>` without `--implementer=codex`). Different bottlenecks.
- Replacing the protocol's review pass with a faster format. The second opinion is the design's reason to exist.
