---
name: iron-build
description: Use when executing an implementation plan - dispatches a fresh subagent per task at the task's routed tier, with spec-compliance then code-quality review after each, and runs the whole plan without handing control back.
---
<!-- GENERATED from skills/ — do not edit -->

# Iron Build

Execute an implementation plan end to end: one fresh subagent per task at that
task's routed tier, spec-compliance review then code-quality review after each,
with the loop owned by the controller until the plan is exhausted.

**Why a fresh subagent:** each implementer gets exactly the context you
construct — the task's full text and the scene around it, nothing inherited
from your session. That keeps them focused, and keeps your own context free for
coordination.

## Controller-Owned Loop

Once this skill starts, the controller owns the whole execution loop. Keep
going until one of these happens:

1. Every plan task is implemented, reviewed, committed, and marked complete,
   and the *Finishing the Branch* step has run
2. An implementer or reviewer reports `BLOCKED` or `NEEDS_CONTEXT`
3. The human explicitly interrupts or redirects the work

**Do not hand control back just because one task completed.** A task is
complete only when:

- implementation is done
- spec compliance review is approved
- code quality review is approved
- the task-scoped work is committed
- the task is marked complete in update_plan

Then immediately dispatch the next task.

## Workspace

Implementation runs in an isolated workspace, never in the tree the planning
conversation happened in.

**Never start implementation on `main` or `master` without explicit human
consent.** Check the current branch before dispatching anything; if it is the
default branch, stop and ask.

Choose where the worktree lives, in priority order:

1. An existing `.worktrees/` or `worktrees/` directory at the repo root. If
   both exist, `.worktrees/` wins.
2. A location named in the project's own agent-instructions file. Use it
   without asking.
3. Otherwise ask the human. Do not guess a location.

For a project-local directory, confirm git ignores it — `git check-ignore -q
.worktrees` — before creating anything. If it is not ignored, add it to
`.gitignore` and commit that first, or the worktree's contents get tracked.

Create the workspace with git worktree add ../<branch> -b <branch>, then:

- Install dependencies if the project has them (`npm install`, `cargo build`,
  `pip install -r requirements.txt`, `go mod download` — detect, don't assume).
- Run the test suite once for a clean baseline. **If the baseline is red,
  report the failures and ask whether to proceed** — otherwise you cannot tell
  your bugs from the ones that were already there.
- Report the workspace path and the baseline result before the first dispatch.

## Per-Task Cycle

Read the plan once, up front. Extract every task's full text plus the context
around it, and put the whole task list into update_plan. Implementers never read
the plan file — you hand them the text.

Then, for each task in plan order:

1. **Record the task's base commit — before you dispatch anything.** Run `git
   rev-parse HEAD` and keep the value; that is this task's `BASE_SHA`. Capture
   it *first*, because once the implementer starts committing there is no way
   to recover where the task began.
2. **Resolve the tier** from the task's `**Tier:**` line — see below.
3. **Dispatch the implementer** at that tier using `./prompts/implementer.md`,
   filled in with the full task text and scene-setting context.
4. **Answer its questions.** If it asks something before or during the work,
   answer completely before letting it proceed.
5. **Handle the reported status** — see below. Only `DONE`, or a
   `DONE_WITH_CONCERNS` whose concerns you have resolved, continues.
6. **Spec-compliance review** with `./prompts/spec-reviewer.md` at the reviewer
   tier. Issues found → the same implementer fixes them → review again. Loop
   until compliant.
7. **Code-quality review** with `./prompts/quality-reviewer.md` at the reviewer
   tier, and only once spec compliance passes. Same fix-and-re-review loop.
8. **Verify the task's work is committed.** The implementer commits its own
   work — see *Who Commits* below. Run `git status --porcelain`; if anything
   is uncommitted, dispatch the implementer back to commit it rather than
   committing for it.
9. **Record the outcome** as an ironmem drawer — see below.
10. **Mark the task complete** in update_plan, then immediately start the next one.

**Passing the review range.** Every reviewer dispatch gets the git range
explicitly: `BASE_SHA` is the value from step 1, and `HEAD_SHA` is `git
rev-parse HEAD` taken at the moment you dispatch that reviewer. Re-read
`HEAD_SHA` before every re-review, so a fix round is inside the range.

**`HEAD~1..HEAD` is not acceptable.** An implementer makes as many commits as
the work needs, so a one-commit range shows the reviewer a fragment of the task
and it will report that fragment clean — a gate that appears to fire and does
not. The range always starts at the recorded `BASE_SHA`.

## Who Commits

The implementer commits its own work, including any fixes it makes during a
review round. That is what makes `BASE_SHA..HEAD` a reviewable range at all,
and it is why `iron-tdd`'s cycle ends in a commit.

The controller does not commit implementation work. Its step 8 is a *check*,
not a commit point: confirm the tree is clean, and send anything left over back
to the implementer that produced it.

Never run two implementers at once; they collide in the same worktree. Never
repair an implementer's work yourself — dispatch the fix back to it, so the fix
lands in the context that produced the code.

After the last task, dispatch one final code-quality review over the whole
implementation, then go to *Finishing the Branch*.

## Resolving a Tier

Every task in the plan carries a `**Tier:**` line. Read it and look the value
up in `./references/tiers.md` — the only file in this skill that differs per
harness.

The tier names are exactly `cheap`, `standard`, `deep`, `frontier`. Anything
else — a typo, a raw model name, a missing line — is a hard error at
plan-parse time. **Stop, name the bad value, and ask.** Never fall back to
`standard`: a typo must not silently route work to the wrong model.

**Reviewer floor.** Reviewers run at least one tier above the implementer, and
never below `standard`. A `cheap` implementation gets a `standard` spec
reviewer; a `deep` one gets a `frontier` reviewer; a `frontier` one gets a
`frontier` reviewer — the floor raises, it never wraps around. Cheap
implementer plus expensive reviewer is the better cost/quality point, because
review is what catches the cheap model's mistakes.

Note what you actually dispatched — model, dispatch path, and effort if one was
passed — because `./references/tiers.md` documents efforts that some dispatch
paths silently ignore. *Recording the Outcome* is where that goes.

## Handling Implementer Status

**DONE** — proceed to spec compliance review.

**DONE_WITH_CONCERNS** — read the concerns before proceeding. If they touch
correctness or scope, address them before review. If they are observations
("this file is getting large"), note them and proceed.

**NEEDS_CONTEXT** — supply the missing context and re-dispatch at the same
tier. A context gap is not a capability gap.

**BLOCKED** — assess the blocker:

1. Context problem → provide more context, re-dispatch at the same tier
2. Reasoning problem → re-dispatch one tier higher
3. Task too large → split it
4. Plan is wrong → stop and escalate to the human

**Escalate on failure.** If the reviewer rejects the same task twice,
re-dispatch the implementer one tier higher — once. If that also fails, stop
and ask the human. At `frontier` there is no higher tier, so escalation there
means stopping and asking the human immediately — never re-run `frontier`
against itself. Never re-dispatch at the same tier with the same context and
expect a different result.

**Never** silently mark a task complete after a `BLOCKED` or `NEEDS_CONTEXT`.

## Recording the Outcome

After each completed task, write one ironmem drawer with a `logical_key` so
the latest state overwrites the previous copy rather than accumulating:

    add_drawer(
      logical_key = "iron-build:<plan-slug>:task-<n>",
      content = {
        "task_shape":    "<one line: what kind of work this was>",
        "tier_assigned": "cheap|standard|deep|frontier",
        "tier_used":     "cheap|standard|deep|frontier",
        "dispatch_path": "workflow|agent",
        "effort_applied": "<value>|null",
        "review_rounds": <int>,
        "escalated":     true|false
      }
    )

`dispatch_path` and `effort_applied` are not bookkeeping. On the plain agent
path, effort is inert (see `./references/tiers.md`), so a drawer that records
an effort it never applied poisons the dataset this table is meant to improve.
Write `null` when no effort was passed.

## Finishing the Branch

A controller may instruct you to stop before this step. When it does: stop
after the final task's review and commit, report what was completed, and
create no pull request. Honoring that instruction is not optional — an
orchestrator that owns its own PR path (such as `/collab`) will treat a PR
created here as a protocol violation.

Otherwise, once every task is implemented, reviewed, committed, and marked
complete:

1. **Verify tests pass.** Run the full suite. If anything fails, stop and
   report; never present integration options over a red tree.
2. **Determine the base branch** — `git merge-base HEAD main`, or ask.
3. **Present exactly these four options and let the human choose.** Don't
   recommend one, don't pick for them, don't pad them with explanation:
   (1) merge back to the base branch locally, (2) push and open a pull request,
   (3) keep the branch as-is, (4) discard this work.
4. **Execute the choice.** Merging locally: merge, re-run the tests on the
   merged result, then delete the branch. Discarding: list the branch, its
   commits, and the worktree path, and require the human to type `discard`
   before anything is deleted.
5. **Clean up the worktree** for options 1 and 4 only. Options 2 and 3 keep it.

Choosing for the human is the failure mode this section exists to prevent.
Present, then wait.

## Red Flags

**Never:**

- Start implementation on `main`/`master` without explicit human consent
- Skip either review, or start the code-quality review before spec compliance
  has passed — that order is not negotiable
- Move to the next task while either review has open issues
- Dispatch two implementation subagents in parallel
- Make a subagent read the plan file — paste the full task text instead
- Skip the scene-setting context; a subagent that doesn't know where its task
  fits builds the wrong thing correctly
- Ignore a subagent's questions, or rush it into implementation
- Accept "close enough" on spec compliance — a reviewer who found issues means
  the task is not done
- Let the implementer's self-review stand in for actual review; both exist
- Fix a subagent's work by hand instead of dispatching the fix back to it
- Re-dispatch at the same tier with the same context and expect a different
  result
- Hand control back to the human between tasks
