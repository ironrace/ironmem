# `/evaluate-issue` — issue-to-workflow router

Source of truth for the `/evaluate-issue` command. Reads a GitHub issue,
scores its complexity, and recommends one of three execution paths:
**DIRECT**, **SUPERPOWERS**, or **COLLAB**. The command is advisory by
default: it prints a verdict and the exact command to run, then asks the
user to confirm before invoking the chosen path.

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
scan reflects the current working tree, not that repo.

## Step 1 — Read the issue

Fetch the issue with the GitHub MCP tool when available
(`mcp__github__issue_read`), otherwise shell out to
`gh issue view <ref> --json number,title,body,labels,comments`.

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
   - changes to a state machine (e.g. `collab/state_machine`)
   - a schema **migration** (`migrations/`, version bumps)
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
| **Decomposability** | Into how many *independent* tasks does it split? |
| **Spec clarity** | Is the issue well-specified, or ambiguous / under-defined? |
| **Verification value** | Would an adversarial second-model review materially de-risk it before merge? |

## Step 4 — Decide (first match wins)

Evaluate in this order. The order is deliberate: DIRECT is the cheapest, so
it is checked first; COLLAB is the most expensive, so it must be positively
justified; SUPERPOWERS is the safe default for everything in between.

### 1. DIRECT — `/plan` + TDD

Choose when **all** hold:

- blast radius ≤ ~2 files, single crate / module
- decomposes to 1 (at most 2 tightly-coupled) steps
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
- large blast radius — roughly **3+ crates** or many interacting modules
- the change genuinely benefits from cross-model **adversarial review**
  before merge (correctness-critical, security-sensitive, protocol-level)

Typical issues: protocol or state-machine changes, migrations, cross-crate
features, anything touching the collab state machine or a public boundary.

### 3. SUPERPOWERS — writing-plans → subagent-driven-development → TDD

The default middle tier. Choose when the issue is neither DIRECT nor COLLAB:

- 2–6 independent tasks
- moderate blast radius (1–2 crates)
- well-enough specified to write a plan up front
- no need for cross-model design review

Typical issues: a feature within an existing subsystem, a multi-file but
mechanical change, a test-coverage expansion.

When a single signal pulls toward COLLAB but the rest sit firmly in
SUPERPOWERS territory, name the tension in the rationale rather than
silently rounding up — the user confirms with that tradeoff visible.

## Step 5 — Output (recommend, then confirm)

Print exactly this shape:

```
Issue #<number>: <title>
Verdict: <DIRECT | SUPERPOWERS | COLLAB>

Why:
 - <signal>: <one-line evidence>
 - <signal>: <one-line evidence>
 ...

Blast radius: <N files, M crates> (<short list of the main ones>)

Recommended path:
  <exact command — see platform table below>

Proceed with this path? [y/N]
```

Then **wait for the user**. On an explicit yes, invoke the recommended path.
On anything else (including silence treated as no), stop without side
effects. This is the only gate — the scan and scoring are read-only.

## Platform path mapping

The verdict (DIRECT / SUPERPOWERS / COLLAB) is platform-agnostic; the
"recommended path" and how it is invoked differ per host.

| Verdict | Claude | Codex |
|---|---|---|
| DIRECT | `/plan` to scope, then the `test-driven-development` skill (`/tdd`) | the `test-driven-development` skill directly |
| SUPERPOWERS | invoke the `writing-plans` skill on the issue spec (flows into `subagent-driven-development`) | invoke the `writing-plans` skill |
| COLLAB | `/collab start <one-line task summary derived from the issue>` | `/collab` is Claude-driven: recommend the user run `/collab start <task>` in a Claude terminal (Codex joins via the protocol) |

For the COLLAB task summary, derive a single imperative line from the issue
title/body (e.g. `add schema migration 012 for code_map TTL`). Do not paste
the whole issue body into the command.

## Invariants

- **Read-only until the confirm gate.** Steps 1–4 never mutate state. The
  only action is invoking the chosen path *after* the user confirms.
- **Never auto-launch.** Even an obvious DIRECT verdict waits for `y`.
- **Recommend one path, not a menu.** Pick the single best fit and justify
  it. The user can override by declining and running something else.
- **Scan, don't trace.** Step 2 is a handful of grep/glob calls, not a
  subagent fan-out. If accurate routing would need deep tracing, say so in
  the rationale and lean toward the more cautious tier (usually COLLAB).
