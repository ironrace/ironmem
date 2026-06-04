---
name: review-pr
description: Review a GitHub PR, GitLab MR, or local branch diff by determining scope, launching applicable focused reviewers in parallel, and synthesizing prioritized findings. Use when the user invokes pr-review-toolkit:review-pr, asks for a PR review, asks to review current branch changes, or asks for parallel/subagent review of a diff.
---

# Review PR

Perform a structured, review-only PR analysis. Prioritize correctness, security, regressions, broken contracts, and missing tests over style.

## Scope Capture

Start by identifying the review target and changed files.

1. If the user provides a PR/MR number or URL, fetch metadata, changed files, CI status, and diff with `gh` or `glab` when available.
2. Otherwise review the current branch against the best base branch:
   - Prefer `main`, then `master`, then the tracked upstream branch.
   - Run `git --no-pager diff <base>...HEAD --stat`.
   - Run `git status --short` and include dirty files in scope if they affect the review.
   - Capture changed file names with `git --no-pager diff <base>...HEAD --name-only`.
3. If the diff is empty, check unstaged/staged changes with `git --no-pager diff --stat` and `git --no-pager diff --cached --stat`.
4. Read relevant changed files and enough neighboring code to verify line references.

Do not rely only on patch hunks when a finding depends on surrounding behavior.

## Agent Fan-Out

When the user explicitly invokes `pr-review-toolkit:review-pr` or asks for subagents/parallel reviewers, spawn applicable read-only agents in parallel. Treat the skill invocation as permission for parallel review. If subagents are unavailable, run the same focused passes locally and say so.

Use `spawn_agent` with `agent_type: "reviewer"` where available. In every prompt, tell the agent:

- This is a read-only review. Do not edit files, stage, commit, or revert.
- The codebase may contain user changes. Do not ask to revert unrelated work.
- Verify file paths and line numbers from the current workspace.
- Return only actionable findings, ordered by severity, plus a short residual-risk note.
- Prefer "no findings" over speculative issues.

Launch all applicable agents in one parallel batch.

## Reviewer Matrix

Always launch these reviewers for non-trivial diffs:

- `code-reviewer`: correctness, logic regressions, error handling, state handling, data flow, user-visible behavior, maintainability issues that can cause bugs.
- `pr-test-analyzer`: missing or weak tests, deleted tests, untested edge cases, insufficient integration/e2e coverage, brittle test updates.

Launch conditionally based on changed files:

- `type-design-analyzer`: TypeScript types, Rust public types/traits, API contracts, schemas, generated types, SDKs, public interfaces, migrations.
- `comment-analyzer`: new or changed comments, doc comments, README/docs/API docs, examples, changelog text, misleading naming or stale docs.
- `security-reviewer`: auth, authorization, secrets, input validation, SQL/command/file/path handling, crypto, payments, PII, network exposure.
- `performance-reviewer`: hot paths, database queries, caching, concurrency, memory allocation, bundle size, expensive loops, streaming or pagination.
- `dependency-reviewer`: lockfiles, package manifests, build tooling, Docker/CI changes, new transitive risk, license or supply-chain concerns.

For tiny diffs, use only the reviewers that match the changed surface. Avoid spawning agents that cannot add signal.

## Agent Prompts

Use the prompt templates in `../../references/reviewer-prompts.md` when composing subagent tasks. Add the concrete base branch, changed files, and any PR metadata gathered during scope capture.

Keep each agent's scope bounded. Do not ask every agent to do the whole review.

## Synthesis

After reviewers finish, merge duplicate findings and independently verify each surviving issue before presenting it. Drop findings that cannot be confirmed from the code.

Final output shape:

```markdown
## Findings

- [severity] path/to/file.ext:line - concise issue title.
  Explain the concrete failure mode and why it matters. Include a targeted fix direction.

## Open Questions

- Only include questions that affect whether the change is correct.

## Review Notes

- Mention reviewers run, tests/commands inspected or run, and residual risk.
```

Severity labels:

- `critical`: exploitable security issue, data loss, auth bypass, build-breaking public contract.
- `high`: likely production bug, broken user flow, major regression, missing required migration/test on risky change.
- `medium`: edge-case bug, incomplete validation, meaningful test gap, confusing API or stale docs that can cause misuse.
- `low`: minor maintainability issue with clear downstream cost.

If there are no findings, state that clearly and list remaining risk or unrun checks.

## Review Boundaries

- Do not include style nits unless they hide a bug or maintenance hazard.
- Do not report "missing tests" without naming the behavior or edge case that is uncovered.
- Do not invent CI results; say when checks were not run or unavailable.
- Do not post comments to GitHub/GitLab unless explicitly asked.
- Do not modify source files as part of review.
