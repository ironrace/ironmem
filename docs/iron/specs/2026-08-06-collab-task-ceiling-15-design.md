# Collab Task Ceiling 15 Design

**Date:** 2026-08-06

**Scope:** Raise the maximum number of execution tasks accepted by one collab issue from 10 to 15, and keep every runtime, prompt, evaluator, and documentation surface synchronized.

**Status:** Approved design, pending implementation plan

## Problem

One collab session currently accepts at most 10 implementation tasks. The runtime enforces that limit through `MAX_TASKS_PER_COLLAB_ISSUE`, while Claude and Codex planning prompts, `/evaluate-issue`, protocol documentation, and drift checks repeat the same boundary. Issue #250 demonstrates the operational cost: related findings must be fragmented into more child issues than are useful for ownership or review.

Changing only `/evaluate-issue` would create a broken contract: the router could recommend a 15-task collab issue that the MCP server still rejects. The ceiling therefore has to move as one cross-surface protocol value.

## Goals

- Accept collab task lists containing 1 through 15 tasks.
- Reject task lists containing 16 or more tasks with an error naming the 15-task maximum.
- Make Claude and Codex planning/review prompts split only when a scope credibly needs 16 or more tasks.
- Make `/evaluate-issue` return `SPLIT` only above 15 tasks and require every proposed child to contain 1–15 tasks.
- Update protocol documentation and drift checks so a stale 10-task surface fails CI.
- Preserve the existing 20-minute maximum per task and every other collab/evaluator invariant.

## Non-goals

- Making the task ceiling configurable at runtime.
- Changing how task independence, acceptance criteria, task ordering, or timeboxes are judged.
- Changing the DIRECT, IRON, or COLLAB routing criteria other than the mandatory SPLIT boundary.
- Reworking existing issue #250 findings, authorization policy, or review semantics beyond the narrow finalization-abort path required to avoid stranding an oversized plan.
- Migrating persisted data; task-list JSON stores the `tasks` array without a schema-bound maximum, and task cardinality is derived from that array at runtime rather than persisted separately.

## Architecture

`crates/ironmem/src/collab/mod.rs::MAX_TASKS_PER_COLLAB_ISSUE` remains the runtime source of truth and changes from 10 to 15. Boundary tests exercise both MCP parsing and direct state-machine entry at 15 and 16 tasks.

Human- and agent-facing surfaces remain explicit prose rather than importing the Rust constant. They change in lockstep across `docs/COLLAB.md`, both plugin harnesses, and the three `/evaluate-issue` mirrors. `scripts/check_collab_turn_templates.py` pins the new phrases so CI catches partial updates.

Rejected alternatives:

- **Raise only `/evaluate-issue`:** rejected because 11–15-task plans would route into collab and then fail server validation.
- **Introduce a runtime configuration knob:** rejected because different clients could plan against different ceilings, weakening the protocol invariant and adding configuration complexity for a fixed policy decision.
- **Remove the ceiling:** rejected because a bounded session is still needed for plan quality, reviewability, and execution time.

## Data Flow

1. `/evaluate-issue` estimates independent tasks. Estimates of 1–15 continue to the selected workflow; estimates above 15 produce `SPLIT`, whose children must each estimate 1–15 tasks.
2. Collab planning prompts require a canonical plan containing at most 15 task headings and block before `PlanLocked` when 16 or more are needed.
3. The task-list bridge submits JSON to the MCP server.
4. `validate_task_list_body` and the state machine compare the submitted count against `MAX_TASKS_PER_COLLAB_ISSUE == 15`.
5. Accepted task lists are persisted in the existing JSON representation. No schema or record migration is required.

## Error Handling

| Condition | Behavior |
|---|---|
| Zero tasks | Preserve the existing validation error requiring at least one task. |
| 1–15 tasks | Accept when all existing shape, ordering, timebox, and acceptance rules pass. |
| 16+ tasks | Reject with the existing split guidance, updated to say “at most 15 tasks.” |
| Planning identifies 16+ tasks | Stop before plan lock and require independently executable child issues. |
| Evaluator estimates 16+ tasks | Return mandatory `SPLIT`; do not launch collab for the parent. |
| A prompt or documentation mirror retains the old ceiling | The collab-template drift linter fails CI. |
| GitHub child creation fails after the protocol change | Preserve `/evaluate-issue` retry keys and partial-failure behavior; this design does not alter issue-write semantics. |

## Testing

| Test | Coverage |
|---|---|
| MCP parser accepts exactly 15 tasks | Proves the public task-list entrypoint permits the new boundary. |
| MCP parser rejects 16 tasks and reports a maximum of 15 | Proves the public entrypoint enforces the new upper bound and message. |
| State machine accepts exactly 15 tasks | Proves direct Rust callers share the new boundary. |
| State machine rejects a canonical 16-task list | Proves runtime validation cannot be bypassed through direct event construction. |
| State machine rejects a declared count that hides a 16-task JSON array | Preserves count-integrity enforcement at the new boundary. |
| Collab template linter requires 15-task phrases | Proves Claude/Codex planning surfaces cannot silently retain 10/11 wording. |
| Evaluate-issue lint mutation test | Proves all evaluator mirrors require the 15-task SPLIT contract. |
| Full Python and Rust suites | Detect unrelated drift and packaging regressions across mirrored plugin assets. |

## Consequences

Related work can stay in larger, more coherent collab issues, reducing tracking and handoff overhead. Runtime enforcement, agent instructions, and evaluator routing continue to agree.

The cost is that a single session may run up to five additional 20-minute implementation tasks, increasing worst-case session duration and review load. The fixed 15-task ceiling also remains a policy constant that requires a coordinated release to change again.
