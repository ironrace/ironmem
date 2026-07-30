---
name: iron-plan
description: Use when you have a spec or requirements for a multi-step task, before touching code. Produces a tier-tagged implementation plan.
---
<!-- GENERATED from skills/ — do not edit -->

# Iron Plan

Turn an approved spec into an implementation plan an engineer can execute without guessing. Write for a skilled developer who has zero context for this codebase and does not know its toolset, its problem domain, or good test design. Give them everything: exact file paths per task, the actual code, exact commands with their expected output, and the docs they would otherwise have to hunt down. DRY, YAGNI, TDD, frequent commits.

**Save plans to:** `docs/iron/plans/YYYY-MM-DD-<feature-name>.md` (a user preference for plan location overrides this default). Check whether that path is gitignored. If it is, the working tree is the only copy — also persist the plan to an ironmem drawer with a `logical_key` so a `git clean` or a fresh clone cannot destroy it mid-execution.

## Task Right-Sizing

If the spec covers several independent subsystems, write one plan per subsystem. Each plan must produce working, testable software on its own.

Within a plan, a task is one coherent change that ends in a commit — a component, an interface, a migration. Right-sized means an implementer can hold it in context and a reviewer can judge it without reading the next task. Too big: it spans three concerns and its steps stop being individually verifiable. Too small: it cannot be committed on its own, or its only test belongs to another task. Order tasks so each one leaves the tree green: a task may depend on an earlier task, never on a later one.

## File Structure

Before defining tasks, map out which files will be created or modified and what each one is responsible for. This is where decomposition decisions get locked in.

- Design units with clear boundaries and well-defined interfaces. Each file should have one clear responsibility.
- You reason best about code you can hold in context at once, and your edits are more reliable when files are focused. Prefer smaller, focused files over large ones that do too much.
- Files that change together should live together. Split by responsibility, not by technical layer.
- In existing codebases, follow established patterns. Don't unilaterally restructure — but if a file a task modifies has grown unwieldy, planning a split is reasonable.

## Plan Document Header

Every plan MUST start with this header:

```markdown
# [Feature Name] Implementation Plan

> **For agentic workers:** Use `iron-build` to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Spec:** `docs/iron/specs/YYYY-MM-DD-<topic>-design.md`

**Goal:** [One sentence describing what this builds]

**Architecture:** [2-3 sentences about approach]

**Tech Stack:** [Key technologies/libraries]

---
```

## Task Structure

````markdown
### Task N: [Component Name]

**Tier:** `standard`

**Files:**
- Create: `exact/path/to/file.py`
- Modify: `exact/path/to/existing.py:123-145`
- Test: `tests/exact/path/to/test.py`

**Interfaces:** [names, signatures, and paths later tasks may rely on]

- [ ] **Step 1: Write the failing test**

```python
def test_specific_behavior():
    assert function(input) == expected
```

- [ ] **Step 2: Run test to verify it fails**

Run: `pytest tests/path/test.py::test_name -v` — Expected: FAIL, "function not defined"

- [ ] **Step 3: Write minimal implementation** — with the real code, in a block, as in Step 1
- [ ] **Step 4: Run test to verify it passes** — same command — Expected: PASS
- [ ] **Step 5: Commit** — with the exact `git add` and `git commit -m` line
````

## Bite-Sized Task Granularity

**Each step is one action (2-5 minutes):**
- "Write the failing test" — step
- "Run it to make sure it fails" — step
- "Implement the minimal code to make the test pass" — step
- "Run the tests and make sure they pass" — step
- "Commit" — step

## Tier Routing

Every task carries a **tier**, never a model name. Write it as a `**Tier:**`
line directly under the task heading. Tiers resolve to a concrete
`(model, effort)` pair in `iron-build/references/tiers.md`, which is the only
file that differs per harness.

| Tier | Task shape |
|---|---|
| `cheap` | Mechanical edits, renames/moves, boilerplate scaffolds, doc formatting, test fixtures |
| `standard` | Single-file feature with a tight spec, straightforward test writing, localized bugfix |
| `deep` | Cross-cutting changes, API and type design, non-trivial algorithmic work |
| `frontier` | Architecture and security escalation, concurrency and `unsafe` Rust, the hardest long-horizon work |

Two rules make routing pay off:

1. **Reviewer floor.** Reviewers run at least one tier above the implementer,
   and never below `standard`. A `cheap` implementation gets a `standard`
   spec-reviewer. Cheap implementer plus expensive reviewer is a better
   cost/quality point than the reverse, because review is what catches the
   cheap model's mistakes.

2. **Escalate on failure.** If a task's reviewer rejects twice, the controller
   re-dispatches the implementer one tier higher, once. If that also fails, it
   stops and asks the human. Under-routing is therefore self-correcting, so
   guess low.

**Never assign `cheap` to a task whose file set will not fit Haiku 4.5's 200K
context / 64K output.** Over-routing costs money; mid-implementation truncation
costs the task. Count the files the task must read, not just the files it
writes.

An unrecognized tier string is a hard error when `iron-build` parses the plan.
There is no default. A typo must not route work to the wrong model.

## No Placeholders

Every step must contain the actual content an engineer needs. These are **plan failures** — never write them:

- "TBD", "TODO", "implement later", "fill in details"
- "Add appropriate error handling" / "add validation" / "handle edge cases"
- "Write tests for the above" (without actual test code)
- "Similar to Task N" (repeat the code — the engineer may be reading tasks out of order)
- Steps that describe what to do without showing how (code blocks required for code steps)
- References to types, functions, or methods not defined in any task

Exact file paths always. Complete code in every step — if a step changes code, show the code. Exact commands with expected output.

## Self-Review

After writing the complete plan, look at the spec with fresh eyes and check the plan against it. You run these three yourself, before dispatching any reviewer.

**1. Spec coverage:** Skim each section and requirement in the spec. Can you point to a task that implements it? List the gaps; a spec requirement with no task gets a task.

**2. Placeholder scan:** Search the plan for every pattern in "No Placeholders" above. Fix what you find.

**3. Type consistency:** Do the types, signatures, and property names used in later tasks match what earlier tasks defined? `clearLayers()` in Task 3 and `clearFullLayers()` in Task 7 is a bug, not a wording difference.

Fix issues inline and move on — no re-review pass.

## Plan Review

Skip for a short plan; worth it for anything an implementer will run unattended. Dispatch a reviewer with spawn_agent(agent_type="worker", model=<model>, reasoning_effort=<effort>, message=<full task text>) carrying this prompt:

```
You are a plan document reviewer. Verify this plan is complete and ready for implementation.
Plan: [PLAN_FILE_PATH]    Spec: [SPEC_FILE_PATH]

Check completeness (placeholders, missing steps), spec alignment (requirements covered, no scope creep), task decomposition (clear boundaries, actionable steps), buildability (could an engineer follow this without getting stuck?), and tier sanity (does each task's tier match the shape of its work?).

**Only flag issues that would cause real problems during implementation.** An implementer building the wrong thing or getting stuck is an issue; wording, style, and nice-to-haves are not. Approve unless there are serious gaps — missing spec requirements, contradictory steps, placeholder content, or tasks too vague to act on.

Output — **Status:** Approved | Issues Found; then **Issues:** `[Task X, Step Y]: [issue] — [why it matters for implementation]`; then **Recommendations (advisory, do not block approval)**.
```

## Handoff

Save the plan, state its path, and hand off to `iron-build` — the single execution path. It runs one fresh agent per task at that task's tier, with spec review and quality review between tasks.
