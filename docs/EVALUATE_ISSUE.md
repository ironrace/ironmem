# `/evaluate-issue` — issue-to-workflow router

Source of truth for the `/evaluate-issue` command. Reads a GitHub issue,
scores its complexity, and recommends an execution path: **DIRECT**,
**IRON**, **COLLAB**, or mandatory **SPLIT** for work that exceeds
collab's 10-task issue budget. The command is advisory by default: it prints
a verdict and the exact next step, then asks the user to confirm before
performing any mutation.

Protocol changes must update all three files in lockstep:

- `docs/EVALUATE_ISSUE.md` (this spec)
- `.claude-plugin/commands/evaluate-issue.md` (Claude command)
- `.codex-plugin/prompts/evaluate-issue.md` (Codex mirror)

## Purpose

Choosing the wrong workflow wastes effort in both directions. Running the
full Claude+Codex `/collab` protocol on a one-line bug fix burns time and
tokens on planning ceremony the change never needed. Hand-coding a
cross-crate migration without a plan or a second reviewer ships avoidable
defects. `/evaluate-issue` makes that routing decision explicit and
repeatable instead of ad hoc.

The output is a recommendation, not an action. The user always confirms
before any path is launched.

## Input

`$ARGUMENTS` is a single issue reference in any of these forms:

- `123`
- `#123`
- `https://github.com/<owner>/<repo>/issues/123`

Resolve the repository from the working tree, never from the user:

- `repo_root` ← `git rev-parse --show-toplevel`
- `owner/repo` ← `gh repo view --json nameWithOwner -q .nameWithOwner`

If `$ARGUMENTS` is empty or is not a recognizable issue reference, print the
usage line and stop:

```
Usage: /evaluate-issue <issue-number | #number | issue-url>
```

If a URL names a different repo than the local checkout, prefer the URL's
repo for the `gh issue view` call but warn the user that the local blast-radius
scan reflects the current working tree, not that repo. A cross-repository
`SPLIT` verdict is advisory-only: do not create child issues or comment on the
parent; ask the user to rerun `/evaluate-issue` from a checkout of that repo.

## Step 1 — Read the issue

Fetch the issue with the GitHub MCP tool when available
(`mcp__github__issue_read`), otherwise shell out to
`gh issue view <ref> --json number,title,body,labels,comments`. Codex has no
GitHub MCP tool, so the Codex mirror uses `gh` directly — keep that host
difference when editing this step.

Capture: `number`, `title`, `body`, `labels[]`, and the text of any
`comments`. Labels and comments often carry the strongest routing signal
(`good first issue`, `breaking-change`, `migration`, `needs-design`,
extended back-and-forth discussion).

## Step 2 — Light repo scan (blast-radius estimate)

This is a quick, deterministic scan — **no subagents, no deep tracing.**
Goal: a rough estimate of how much code the issue touches.

1. Extract candidate identifiers from the issue text: file paths, module
   names, type/function names, crate names, error strings.
2. Use `grep`/`glob` (ripgrep / Glob tool) to locate those identifiers and
   estimate:
   - **files** likely touched
   - **crates / top-level modules** spanned (e.g. distinct `crates/*`)
   - whether a **test surface** exists for the area
3. Note structural red flags that point to design judgment:
   - changes to a public API, trait, or exported contract
   - changes to a state machine (e.g. `crates/ironmem/src/collab/state_machine`)
   - a schema **migration** (`crates/*/migrations/`, version bumps)
   - a **new subsystem / crate**

Keep this to a handful of searches. If the issue is purely prose (docs,
process, a question), the scan may legitimately find nothing — that is a
signal in itself (usually DIRECT).

## Step 3 — Score the five signals

Judge each signal from the issue text plus the scan:

| Signal | Question |
|---|---|
| **Blast radius** | How many files / crates / modules will change? |
| **Design judgment** | Does it require architectural decisions — public API/contract, state machine, migration, new subsystem? |
| **Decomposability** | Into how many *independent* execution tasks does it split? An estimate above 10 requires `SPLIT`. |
| **Spec clarity** | Is the issue well-specified, or ambiguous / under-defined? |
| **Security / review depth** | Does it touch security-sensitive code (auth, credentials, billing, user data)? Drives the post-implementation review tier, not the execution path. |

## Step 4 — Decide (first match wins)

Evaluate in this order. The hard `SPLIT` task-budget gate comes first; then
DIRECT is the cheapest route, COLLAB is the most expensive and must be
positively justified, and IRON is the safe default for everything in
between.

The numeric thresholds below are guidance, not bright lines, and they
intentionally touch at the seams. When counts overlap, the deciding factor
is **task independence, not the number**: a single unit of tightly-coupled
work routes DIRECT; two or more *independently shippable* tasks route up to
IRON. Likewise the COLLAB **design-judgment** triggers dominate the
crate-count range — a purely mechanical change spanning 3+ crates (e.g. a
rename) is IRON, not COLLAB. Security sensitivity alone does not
justify COLLAB — it drives the review tier recommendation instead.

### 0. SPLIT — create smaller issues before routing

Choose **SPLIT** when the issue credibly requires **more than 10 independent
execution tasks**. This is a hard ceiling: do not route the parent issue to
COLLAB, even if it has protocol-level risk or needs adversarial review.

Before the confirmation gate, propose 2+ independently executable child
issues. Every child must have a focused outcome, concrete acceptance criteria,
explicit dependencies (if any), and an estimated **1–10** execution tasks.
Keep the original issue open as the tracking parent. Do not evade the ceiling
by merging unrelated tasks, weakening acceptance criteria, or silently
discarding scope.

### 1. DIRECT — `/plan` + TDD

Choose when **all** hold:

- blast radius ≤ ~2 files, single crate / module
- one unit of work — 1 task, or 2 steps too tightly coupled to ship apart
- **no** architectural / contract / migration / new-subsystem decision
- well-specified

Typical issues: a localized bug fix, a small helper, a doc or config fix, a
contained refactor, a single added test.

### 2. COLLAB — `/collab start`

Choose when **any** hold:

- requires real design judgment — public API/contract change, state-machine
  change, schema migration, or a new subsystem
- requirements are ambiguous and benefit from a second, independent
  perspective before committing to a plan
- large blast radius — roughly **3+ crates** or many tightly interacting
  modules where the changes cannot be planned independently

Security sensitivity alone does not justify COLLAB. Security concerns are
handled by the review tier recommendation (typically `/ultrareview-local`),
not by adding planning overhead.

Typical issues: protocol or state-machine changes, migrations, cross-crate
features, anything touching the collab state machine or a public boundary.

### 3. IRON — iron-plan → iron-build

The default middle tier. Choose when the issue is neither DIRECT nor COLLAB:

- 2–6 *independently shippable* tasks
- moderate blast radius (1–2 crates)
- well-enough specified to write a plan up front
- no need for cross-model design review

Typical issues: a feature within an existing subsystem, a multi-file but
mechanical change, a test-coverage expansion.

When a single signal pulls toward COLLAB but the rest sit firmly in
IRON territory, name the tension in the rationale rather than
silently rounding up — the user confirms with that tradeoff visible.

## Step 5 — Output (recommend, then confirm)

Print exactly this shape:

```
Issue #<number>: <title>
Verdict: <DIRECT | IRON | COLLAB | SPLIT>
Task estimate: <N | N+> independent execution tasks

Why:
 - <signal>: <one-line evidence>
 - <signal>: <one-line evidence>
 ...

Blast radius: <N files, M crates> (<short list of the main ones>)

Recommended path:
  <exact command — see platform table below>

Recommended review:
  <review command — see review tier table below>

For a `SPLIT` verdict, insert before the confirmation line:

Child issues:
  1. <title> — <scope, acceptance summary, 1–10 task estimate, dependencies>
  2. <title> — <scope, acceptance summary, 1–10 task estimate, dependencies>
  ...

Proceed with this path? [y/N]
```

Then **wait for the user**. On an explicit yes, invoke the recommended path.
For `SPLIT`, create the proposed child GitHub issues only when the issue repo
matches the local checkout. Preserve relevant parent labels, include
`Parent: #<number>` in each child body, and add their links in one comment on
the still-open parent issue. If the repos differ, report that the proposal is
advisory-only and ask the user to rerun from the issue repository. If any child
creation fails, report the failure and do not start collab. On anything else
(including silence treated as no), stop without side effects. This is the only
gate — the scan and scoring are read-only.

Make confirmed `SPLIT` writes retry-safe. For every proposed child ordinal,
include `Split-child-key: <owner>/<repo>#<parent-number>:<ordinal>` in its
body. Before creating it, search the local issue repository for that exact key
and reuse an existing match. Before writing the parent summary, read its
comments and create or update exactly one comment carrying
`Split-parent-key: <owner>/<repo>#<parent-number>` and the full child-link
list. A partial failure can then be retried without duplicate child issues or
parent comments.

## Platform path mapping

The verdict (DIRECT / IRON / COLLAB / SPLIT) is platform-agnostic; the
"recommended path" and how it is invoked differ per host.

| Verdict | Claude | Codex |
|---|---|---|
| DIRECT | `/plan` to scope, then the `iron-tdd` skill | the `iron-tdd` skill directly |
| IRON | invoke the `iron-plan` skill on the issue spec (flows into `iron-build`) | invoke the `iron-plan` skill |
| COLLAB | `/collab start <one-line task summary derived from the issue>` | `/collab` is Claude-driven: recommend the user run `/collab start <task>` in a Claude terminal (Codex joins via the protocol) |
| SPLIT | create the confirmed child issues, then re-run `/evaluate-issue` for each child | create the confirmed child issues, then re-run `/evaluate-issue` for each child |

For the COLLAB task summary, derive a single imperative line from the issue
title/body (e.g. `add schema migration 012 for code_map TTL`). Do not paste
the whole issue body into the command.

## Review tiers

Based on change complexity and security sensitivity, recommend one
post-implementation review. This is independent of the execution-path verdict
— a DIRECT fix in auth code still gets a Deep review.

| Tier | When | Command |
|---|---|---|
| Standard | Small, contained changes with no security surface | `/code-review` |
| Thorough | Multi-file changes, moderate complexity | `/pr-review-toolkit:review-pr` |
| Deep | Complex changes, security-sensitive code (auth, credentials, billing, user data), or high blast radius | `/ultrareview-local` |

Security-sensitive changes always warrant at minimum Thorough, and typically
Deep, regardless of the execution-path verdict.

## Invariants

- **Read-only until the confirm gate.** Steps 1–4 never mutate state. The
  only action is invoking the chosen path *after* the user confirms.
- **Never auto-launch.** Even an obvious DIRECT verdict waits for `y`.
- **Task-budget first.** An estimate above 10 tasks always yields `SPLIT`;
  never launch collab for the oversized parent issue.
- **Recommend one path, not a menu.** Pick the single best fit and justify
  it. The user can override by declining and running something else.
- **Scan, don't trace.** Step 2 is a handful of grep/glob calls, not a
  subagent fan-out. If accurate routing would need deep tracing, say so in
  the rationale and lean toward the more cautious tier (usually COLLAB).
