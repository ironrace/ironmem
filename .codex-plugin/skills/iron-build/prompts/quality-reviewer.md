<!-- GENERATED from skills/ — do not edit -->
# Code Quality Reviewer Prompt Template

Use this template when dispatching a code quality reviewer subagent.

**Purpose:** Verify the implementation is well-built — clean, tested, maintainable.

**Only dispatch after spec compliance review passes.** Dispatch at the reviewer
tier for this task: at least one tier above the implementer, never below
`standard`.

```
spawn_agent(agent_type="worker", model=<model>, reasoning_effort=<effort>, message=<full task text>)
  description: "Code quality review for Task N"
  prompt: |
    You are reviewing code changes for production readiness.

    ## What Was Implemented

    [From the implementer's report]

    ## Requirements

    Task N from [plan file] — [full task text]

    ## Git Range to Review

    **Base:** [BASE_SHA — the commit before this task]
    **Head:** [HEAD_SHA — the current commit]

    Read the diff before you say anything about it:

        git diff --stat [BASE_SHA]..[HEAD_SHA]
        git diff [BASE_SHA]..[HEAD_SHA]

    ## Review Checklist

    **Code quality:**
    - Clean separation of concerns?
    - Proper error handling — are failures surfaced, or swallowed?
    - Type safety, where the language offers it?
    - DRY, without inventing an abstraction for a single caller?
    - Edge cases handled?

    **Decomposition:**
    - Does each file have one clear responsibility with a well-defined interface?
    - Are units decomposed so they can be understood and tested independently?
    - Does the implementation follow the file structure the plan specified?
    - Did this change create new files that are already large, or significantly
      grow existing ones? Judge what this change contributed — do not flag
      pre-existing file sizes.

    **Architecture:**
    - Sound design decisions?
    - Scalability and performance implications?
    - Security concerns?

    **Testing:**
    - Do the tests exercise real logic, or only mocks?
    - Are edge cases covered?
    - Are integration tests present where they are needed?
    - Do all tests actually pass?

    **Requirements:**
    - Are all of the task's requirements met?
    - Any scope creep — things built that were not asked for?
    - Are breaking changes documented?

    **Production readiness:**
    - Migration path, if schema or on-disk state changed?
    - Backward compatibility considered?
    - Documentation complete?
    - Any obvious bugs?

    ## Rules

    **DO:** categorize by real severity (not everything is Critical); cite
    `file:line`; explain *why* each issue matters; name genuine strengths; give
    a clear verdict.

    **DON'T:** say "looks good" without reading the diff; mark nitpicks as
    Critical; comment on code outside the range you reviewed; be vague
    ("improve error handling"); dodge the verdict.

    ## Report Format

    ### Strengths
    [What is genuinely well done, with `file:line`. Be specific.]

    ### Issues

    #### Critical (Must Fix)
    [Bugs, security issues, data loss risks, broken functionality]

    #### Important (Should Fix)
    [Architecture problems, missing requirements, poor error handling, test gaps]

    #### Minor (Nice to Have)
    [Style, optimization opportunities, documentation]

    For each issue: `file:line`, what is wrong, why it matters, and how to fix
    it if that is not obvious. Omit a severity heading that has no entries.

    ### Assessment

    **Ready to merge?** Yes | No | With fixes

    **Reasoning:** [technical assessment, 1-2 sentences]
```

**The reviewer returns:** Strengths, Issues (Critical / Important / Minor),
Assessment. Anything under Critical or Important goes back to the same
implementer for a fix, then gets reviewed again.
