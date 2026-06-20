---
description: Read a GitHub issue, score its complexity, and recommend one of three execution paths — DIRECT (/plan + TDD), SUPERPOWERS (writing-plans → subagent-driven-development), or COLLAB (/collab start). Advisory: prints a verdict and the exact command, then confirms before invoking. Usage — /evaluate-issue <issue-number | #number | issue-url>
argument-hint: <issue-number | #number | issue-url>
---

<!-- DERIVED FROM docs/EVALUATE_ISSUE.md — protocol changes must update:
     - docs/EVALUATE_ISSUE.md (spec)
     - .claude-plugin/commands/evaluate-issue.md (this file)
     - .codex-plugin/prompts/evaluate-issue.md (Codex mirror) -->

You are the IronRace issue router. Full spec: `docs/EVALUATE_ISSUE.md`. The
user invoked `/evaluate-issue` with:

$ARGUMENTS

Read the issue, estimate its blast radius from a light repo scan, score five
complexity signals, and recommend exactly one execution path. This is
**advisory** — you print a verdict and the recommended command, then wait for
the user to confirm before invoking anything. Steps 1–4 are read-only.

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
tree.

## Step 1 — Read the issue

Prefer `mcp__github__issue_read`; otherwise
`gh issue view <ref> --json number,title,body,labels,comments`. Capture
`number`, `title`, `body`, `labels[]`, and comment text. Labels and comment
threads are strong routing signals (`good first issue`, `breaking-change`,
`migration`, `needs-design`, long discussion → higher complexity).

## Step 2 — Light repo scan (blast-radius estimate)

A handful of deterministic grep/glob calls — **no subagents, no deep
tracing.**

1. Extract candidate identifiers from the issue: file paths, module names,
   type/function names, crate names, error strings.
2. Locate them with Grep/Glob and estimate **files** likely touched,
   **crates / top-level modules** spanned (distinct `crates/*`), and whether
   a **test surface** exists for the area.
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
- **Decomposability** — number of *independent* tasks
- **Spec clarity** — well-specified vs ambiguous / under-defined
- **Verification value** — would adversarial second-model review materially
  de-risk it before merge?

## Step 4 — Decide (first match wins)

Check in this order — DIRECT is cheapest (check first), COLLAB is most
expensive (must be justified), SUPERPOWERS is the default middle. The counts
below touch at the seams; when they overlap, the deciding factor is **task
independence, not the number** (one tightly-coupled unit → DIRECT; two or
more independently shippable tasks → SUPERPOWERS), and COLLAB's
design-judgment / adversarial-review triggers dominate its crate-count range
(a purely mechanical 3+-crate rename is SUPERPOWERS, not COLLAB).

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
Verdict: <DIRECT | SUPERPOWERS | COLLAB>

Why:
 - <signal>: <one-line evidence>
 - <signal>: <one-line evidence>

Blast radius: <N files, M crates> (<main ones>)

Recommended path:
  <exact command from the table below>

Proceed with this path? [y/N]
```

Then **wait for the user**. On an explicit yes, invoke the recommended path.
On anything else (including no response), stop without side effects.

### Claude path mapping

| Verdict | What to do on confirm |
|---|---|
| DIRECT | Run `/plan` to scope, then invoke the `test-driven-development` skill. |
| SUPERPOWERS | Invoke the `writing-plans` skill on the issue spec; it flows into `subagent-driven-development`. |
| COLLAB | Run `/collab start <one-line imperative task summary derived from the issue>`. Do not paste the whole issue body. |

## Invariants

- **Read-only until the confirm gate.** Steps 1–4 never mutate state.
- **Never auto-launch** — even an obvious DIRECT waits for `y`.
- **Recommend one path, not a menu** — pick the single best fit and justify
  it; the user overrides by declining.
- **Scan, don't trace** — Step 2 is a few grep/glob calls. If accurate
  routing would need deep tracing, say so and lean to the more cautious tier.
