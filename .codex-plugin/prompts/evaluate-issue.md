---
description: Read a GitHub issue, score its complexity, and recommend DIRECT (TDD), SUPERPOWERS (writing-plans → subagent-driven-development), COLLAB (Claude-driven /collab start), or mandatory SPLIT above 10 tasks. Advisory: prints a verdict and the exact next step, then confirms before invoking. Usage — /evaluate-issue <issue-number | #number | issue-url>
---

<!-- DERIVED FROM docs/EVALUATE_ISSUE.md — protocol changes must update all
three in lockstep: docs/EVALUATE_ISSUE.md, .claude-plugin/commands/evaluate-issue.md,
and this file (.codex-plugin/prompts/evaluate-issue.md). -->

You are the IronRace issue router, running as **Codex**. Full spec:
`docs/EVALUATE_ISSUE.md`. The user invoked `/evaluate-issue` with:

$ARGUMENTS

Read the issue, estimate its blast radius from a light repo scan, score five
complexity signals, and recommend exactly one execution path. This is
**advisory** — print a verdict and the recommended next step, then wait for
the user to confirm before invoking anything. Steps 1–4 are read-only.

> Codex tool mapping: use your native shell for `git`/`gh`/`grep`, your file
> search for glob, and load skills natively from `~/.codex/skills`. Interpret
> any "invoke the <name> skill" instruction as activating that skill.

## Parse the argument

`$ARGUMENTS` must be a single issue reference: `123`, `#123`, or a full
`https://github.com/<owner>/<repo>/issues/123` URL. If it is empty or
unrecognizable, print this and stop:

```
Usage: /evaluate-issue <issue-number | #number | issue-url>
```

Resolve the repo from the working tree, never from the user:

- `repo_root` ← `git rev-parse --show-toplevel`
- `owner/repo` ← `gh repo view --json nameWithOwner -q .nameWithOwner`

If a URL names a different repo than the local checkout, query that repo for
the issue but warn that the blast-radius scan reflects the local working
tree. A cross-repository `SPLIT` verdict is advisory-only: do not create child
issues or comment on the parent; ask the user to rerun `/evaluate-issue` from a
checkout of that repo.

## Step 1 — Read the issue

Run `gh issue view <ref> --json number,title,body,labels,comments`. Capture
`number`, `title`, `body`, `labels[]`, and comment text. Labels and comment
threads are strong routing signals (`good first issue`, `breaking-change`,
`migration`, `needs-design`, long discussion → higher complexity).

## Step 2 — Light repo scan (blast-radius estimate)

A handful of deterministic grep/glob calls — **no sub-agent fan-out, no deep
tracing.**

1. Extract candidate identifiers from the issue: file paths, module names,
   type/function names, crate names, error strings.
2. Locate them and estimate **files** likely touched, **crates / top-level
   modules** spanned (distinct `crates/*`), and whether a **test surface**
   exists for the area.
3. Flag structural design-judgment markers: public API / trait / exported
   contract changes, state-machine changes (e.g.
   `crates/ironmem/src/collab/state_machine`), schema **migrations**
   (`crates/*/migrations/`, version bumps), or a **new subsystem / crate**.

A prose-only issue (docs, process, a question) may find nothing — that is
itself a DIRECT signal.

## Step 3 — Score five signals

- **Blast radius** — files / crates / modules changed
- **Design judgment** — architectural decisions (API/contract, state
  machine, migration, new subsystem)?
- **Decomposability** — number of *independent* execution tasks; an estimate
  above 10 requires `SPLIT`
- **Spec clarity** — well-specified vs ambiguous / under-defined
- **Verification value** — would adversarial second-model review materially
  de-risk it before merge?

## Step 4 — Decide (first match wins)

Check in this order — the hard `SPLIT` task-budget gate comes first, DIRECT is
the cheapest route, COLLAB is the most expensive and must be justified, and
SUPERPOWERS is the default middle. The counts below touch at the seams; when
they overlap, the deciding factor is **task independence, not the number**
(one tightly-coupled unit → DIRECT; two or more independently shippable tasks
→ SUPERPOWERS), and COLLAB's
design-judgment / adversarial-review triggers dominate its crate-count range
(a purely mechanical 3+-crate rename is SUPERPOWERS, not COLLAB).

0. **SPLIT** — choose before every other route when the issue credibly needs
   **more than 10 independent execution tasks**. Do not start collab for the
   parent. Propose 2+ independently executable child issues, each with focused
   scope, acceptance criteria, dependencies, and a 1–10-task estimate. Never
   merge unrelated work, weaken acceptance criteria, or discard scope merely to
   fit the limit. Keep the parent open as the tracking issue.

1. **DIRECT** — choose when **all** hold: ≤ ~2 files / single crate / module;
   one unit of work (1 task, or 2 steps too coupled to ship apart); no
   architectural / contract / migration / new-subsystem decision;
   well-specified. (Localized bug fix, small helper, doc/config fix, contained
   refactor, single added test.)

2. **COLLAB** — choose when **any** hold: requires real design judgment
   (public API/contract, state-machine, schema migration, new subsystem);
   ambiguous requirements that benefit from a second independent perspective;
   large blast radius (~3+ crates or many interacting modules); genuine value
   in cross-model adversarial review before merge (correctness-critical,
   security-sensitive, protocol-level). (Protocol/state-machine changes,
   migrations, cross-crate features.)

3. **SUPERPOWERS** — the default middle when neither above fits: 2–6
   independently shippable tasks; moderate blast radius (1–2 crates);
   plannable up front; no cross-model design review needed. (A feature within
   an existing subsystem, a multi-file mechanical change, test-coverage
   expansion.)

When one signal pulls toward COLLAB but the rest sit firmly in SUPERPOWERS,
name the tension in the rationale rather than silently rounding up.

## Step 5 — Output, then confirm

Print exactly this shape:

```
Issue #<number>: <title>
Verdict: <DIRECT | SUPERPOWERS | COLLAB | SPLIT>
Task estimate: <N | N+> independent execution tasks

Why:
 - <signal>: <one-line evidence>
 - <signal>: <one-line evidence>

Blast radius: <N files, M crates> (<main ones>)

Recommended path:
  <exact next step from the table below>

For `SPLIT`, insert before the confirmation line:

Child issues:
  1. <title> — <scope, acceptance summary, 1–10 task estimate, dependencies>
  2. <title> — <scope, acceptance summary, 1–10 task estimate, dependencies>
  ...

Proceed with this path? [y/N]
```

Then **wait for the user**. On an explicit yes, invoke the recommended path.
For `SPLIT`, create the proposed child GitHub issues only when the issue repo
matches the local checkout. Preserve relevant parent labels, add
`Parent: #<number>` to each child body, and comment their links on the
still-open parent. If the repos differ, report that the proposal is
advisory-only and ask the user to rerun from the issue repository. If a
creation fails, report it and do not start collab. On anything else (including
no response), stop without side effects.

Make confirmed `SPLIT` writes retry-safe. For every proposed child ordinal,
include `Split-child-key: <owner>/<repo>#<parent-number>:<ordinal>` in its
body. Before creating it, search the local issue repository for that exact key
and reuse an existing match. Before writing the parent summary, read its
comments and create or update exactly one comment carrying
`Split-parent-key: <owner>/<repo>#<parent-number>` and the full child-link
list. A partial failure can then be retried without duplicate child issues or
parent comments.

### Codex path mapping

| Verdict | What to do on confirm |
|---|---|
| DIRECT | Invoke the `test-driven-development` skill directly. |
| SUPERPOWERS | Invoke the `writing-plans` skill on the issue spec; it flows into `subagent-driven-development`. |
| COLLAB | `/collab` is Claude-driven. Recommend the user run `/collab start <one-line imperative task summary derived from the issue>` in a Claude terminal; Codex joins via the protocol. Do not paste the whole issue body. |
| SPLIT | Create the confirmed child issues, then run `/evaluate-issue` for each child. |

## Invariants

- **Read-only until the confirm gate.** Steps 1–4 never mutate state.
- **Never auto-launch** — even an obvious DIRECT waits for `y`.
- **Task-budget first** — an estimate above 10 tasks always yields `SPLIT`;
  never launch collab for the oversized parent issue.
- **Recommend one path, not a menu** — pick the single best fit and justify
  it; the user overrides by declining.
- **Scan, don't trace** — Step 2 is a few grep/glob calls. If accurate
  routing would need deep tracing, say so and lean to the more cautious tier.
