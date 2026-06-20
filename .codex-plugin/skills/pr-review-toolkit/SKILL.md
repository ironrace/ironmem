---
name: pr-review-toolkit
description: Review pull requests and branch diffs with a coordinated multi-agent workflow. Use when the user asks for PR review, branch review, code review of current changes, or explicitly invokes pr-review-toolkit:review-pr, especially when they want parallel focused reviewers for code correctness, test coverage, type/API design, comments/docs, security, or performance.
---

# PR Review Toolkit

Use this skill as a router for PR review workflows. For the actual review protocol, load `skills/review-pr/SKILL.md`.

The default behavior is a review-only pass: inspect the diff, run lightweight commands as needed, spawn focused read-only reviewers when the user has explicitly requested this skill or parallel/subagent review, then synthesize findings. Do not edit code, stage files, commit, or post GitHub comments unless the user explicitly asks.

For IronRace Collab, this skill remains the read-only finding pass. The surrounding `/collab` prompt may independently verify the synthesized findings and then dispatch separate fix subagents in isolated worktrees before sending `review_fix_global`; that fix fan-out is outside this skill.
