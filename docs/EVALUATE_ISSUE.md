# `/evaluate-issue` — issue-to-workflow router

Source of truth for the `/evaluate-issue` command. Reads a GitHub issue,
scores its complexity, and recommends an execution path: mandatory **SPLIT**
for work that exceeds collab's 15-task issue budget, then **AUTOPILOT**,
**DIRECT**, **GOAL**, **COLLAB**, or **IRON** — or one ordered two-phase
verdict such as `COLLAB → GOAL`. The command is advisory by default: it prints
a verdict and the exact next step, then asks the user to confirm before
performing any mutation. It never launches the two unattended routes (GOAL,
AUTOPILOT) itself.

Protocol changes must update all three files in lockstep:

- `docs/EVALUATE_ISSUE.md` (this spec)
- `.claude-plugin/commands/evaluate-issue.md` (Claude command)
- `.codex-plugin/prompts/evaluate-issue.md` (Codex mirror)

## Purpose

Choosing the wrong workflow wastes effort in both directions. Running the
full Claude+Codex `/collab` protocol on a one-line bug fix burns time and
tokens on planning ceremony the change never needed. Hand-coding a
cross-crate migration without a plan or a second reviewer ships avoidable
defects. Keeping a human attached to mechanical, gate-verifiable work that a
bounded `/goal` loop or an Autopilot dispatch could finish alone spends
attention the work never needed. `/evaluate-issue` makes that routing
decision explicit and repeatable instead of ad hoc.

The output is a recommendation, not an action. The user always confirms
before any path is launched, and the user runs the unattended routes
themselves.

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
4. Read the unattended-route prerequisites for the **issue's** repo
   (`<owner>/<repo>` of the issue, which differs from the local checkout when
   a URL named another repo). All are reads; none creates, labels, or
   onboards anything:
   - **Approved gate config.** Call the ironmem MCP tool
     `get_drawer(wing="autopilot", room="backlog-lineage", logical_key="gate-config:<slug>")`,
     where `<slug>` is `<owner>/<repo>` with every character other than
     `A-Z`, `a-z`, `0-9`, `-`, `_`, `.` replaced by `-`
     (`ironrace/ironmem` → `ironrace-ironmem`). The repo qualifies only when
     the drawer is found and its JSON `state` is `"approved"`; keep its
     `gate_commands` for a GOAL command. Not found, `"pending"`, or the tool
     being unavailable all mean "not approved".
   - **Agent labels.** Run
     `gh label list --repo <owner>/<repo> --search "agent:" --limit 100 --json name -q '.[].name'`.
     The repo qualifies only when the output contains all three of
     `agent:ready`, `agent:blocked`, `agent:exhausted`.
   - **Risk label.** Run
     `gh label list --repo <owner>/<repo> --search "risk:" --limit 100 --json name -q '.[].name'`.
     It must list `risk:<expected class>`. Autopilot takes the merge class from
     the issue's `risk:*` label, so without it the PR is held.

Keep this to a handful of searches. If the issue is purely prose (docs,
process, a question), the scan may legitimately find nothing — that is a
signal in itself (usually DIRECT, or AUTOPILOT in an onboarded repo).

## Step 3 — Score the six signals

Judge each signal from the issue text plus the scan:

| Signal | Question |
|---|---|
| **Blast radius** | How many files / crates / modules will change? |
| **Design judgment** | Does it require architectural decisions — public API/contract, state machine, migration, new subsystem? |
| **Decomposability** | Into how many *independent* execution tasks does it split? An estimate above 15 requires `SPLIT`. |
| **Spec clarity** | Is the issue well-specified, or ambiguous / under-defined? |
| **Unattended fit** | Could the repo's gate alone prove the work done, with no judgment left for a human? Which Autopilot risk class would the expected diff carry — `documentation`, `dependency_bump`, `mechanical_rename`, `test_only`, `logic`, `protocol`, `security`, or `public_api`? Drives AUTOPILOT, GOAL, and the phase split. |
| **Security / review depth** | Does it touch security-sensitive code (auth, credentials, billing, user data)? Drives the post-implementation review tier, not the execution path. |

## Step 4 — Decide (first match wins)

Evaluate in this order. The hard `SPLIT` task-budget gate comes first. Then
AUTOPILOT, because when it is eligible no human needs to stay attached at
all, and its eligibility is narrow by construction. DIRECT is the cheapest
attended route; GOAL is the unattended loop for work too large for DIRECT
but with no judgment left in it; COLLAB is the most expensive and must be
positively justified; IRON is the safe default for everything in between. A
COLLAB or IRON verdict then gets one more check, the phase split (rule 6).

The numeric thresholds below are guidance, not bright lines, and they
intentionally touch at the seams. When counts overlap, the deciding factor
is **task independence, not the number**: a single unit of tightly-coupled
work routes DIRECT; two or more *independently shippable* tasks route up to
IRON. Likewise the COLLAB **design-judgment** triggers dominate the
crate-count range — a purely mechanical change spanning 3+ crates (e.g. a
rename) is IRON, not COLLAB. Security sensitivity alone does not
justify COLLAB — it drives the review tier recommendation instead.

### 0. SPLIT — create smaller issues before routing

Choose **SPLIT** when the issue credibly requires **more than 15 independent
execution tasks**. This is a hard ceiling: do not route the parent issue to
COLLAB, even if it has protocol-level risk or needs adversarial review.

Before the confirmation gate, propose 2+ independently executable child
issues. Every child must have a focused outcome, concrete acceptance criteria,
explicit dependencies (if any), and an estimated **1–15** execution tasks.
Keep the original issue open as the tracking parent. Do not evade the ceiling
by merging unrelated tasks, weakening acceptance criteria, or silently
discarding scope.

### 1. AUTOPILOT — `ironmem autopilot lead`

Choose when **all** hold:

- the issue's repo has an approved gate config, all three `agent:*` labels,
  and the `risk:<expected class>` label (Step 2, item 4)
- the expected diff falls inside Autopilot's auto-merge envelope.
  AUTOPILOT auto-merges only these classes: `documentation`, `dependency_bump`, `mechanical_rename`, `test_only`.
- an implementer holding only the issue body can finish it: no open question
  a human must answer, no acceptance criterion a gate cannot check
- one issue's worth of work: one Autopilot dispatch is one branch and one PR

Never recommend AUTOPILOT when the expected class is `logic`, `protocol`, `security`, or `public_api`.
`decide_merge` holds every such PR for a human regardless of the reviewer's
PASS (`HoldForHuman(HighRiskClass)` in
`crates/ironmem/src/autopilot/review.rs`), so the dispatch would spend a run
to hand back a PR a human still has to read. Name the expected class in the
rationale. When unsure between an envelope class and `logic`, the class is
`logic`. The router adapts to this envelope; it never relaxes it.

When the issue otherwise fits AUTOPILOT but a prerequisite is missing, add a
`Missing AUTOPILOT prerequisite:` line to *Why* naming each missing one and
the command that supplies it, then continue down the table:

- no approved gate config → `ironmem autopilot onboard <owner>/<repo> --path <checkout>`, then `ironmem autopilot approve <owner>/<repo>`
- agent labels missing → `ironmem autopilot labels <owner>/<repo>`
- `risk:<expected class>` label missing → `gh label create "risk:<expected class>" --repo <owner>/<repo>`

Typical issues, in an onboarded repo: a docs fix, a dependency bump, a pure
rename, a test-only coverage gap.

### 2. DIRECT — `/plan` + TDD

Choose when **all** hold:

- blast radius ≤ ~2 files, single crate / module
- one unit of work — 1 task, or 2 steps too tightly coupled to ship apart
- **no** architectural / contract / migration / new-subsystem decision
- well-specified

Typical issues: a localized bug fix, a small helper, a doc or config fix, a
contained refactor, a single added test.

### 3. GOAL — a bounded `/goal` loop

Choose when **all** hold:

- the issue's repo has an approved gate config (Step 2, item 4). The loop's
  "done" is that config's `gate_commands`, so it cannot disagree with what CI
  checks
- "done" is fully expressible as that gate passing with the work committed —
  no acceptance criterion needs a human's judgment to verify
- the work is mechanical or high-volume — many files of one shape, data or
  terminology authoring, fixture updates, wiring an already-decided design
  through — rather than judgment-shaped
- **no** architectural / contract / migration / new-subsystem decision is
  left open

GOAL is the unattended route for work outside the auto-merge envelope (a
`logic` diff that is still mechanical) or in a repo without the `agent:*`
labels: the loop runs alone, and a human reviews and merges its PR.

When the issue otherwise fits GOAL but the repo has no approved gate config,
add a `Missing GOAL prerequisite:` line to *Why* naming
`ironmem autopilot onboard <owner>/<repo> --path <checkout>` then
`ironmem autopilot approve <owner>/<repo>`, and continue down the table
(usually to IRON).

Typical issues: applying a decided pattern across many files, authoring a
large table of config or terminology, bulk fixture regeneration.

### 4. COLLAB — `/collab start`

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

### 5. IRON — iron-plan → iron-build

The default middle tier. Choose when no rule above matches:

- 2–6 *independently shippable* tasks
- moderate blast radius (1–2 crates)
- well-enough specified to write a plan up front
- no need for cross-model design review

Typical issues: a feature within an existing subsystem, a multi-file but
mechanical change in a repo with no approved gate config, a test-coverage
expansion.

When a single signal pulls toward COLLAB but the rest sit firmly in
IRON territory, name the tension in the rationale rather than
silently rounding up — the user confirms with that tradeoff visible.

### 6. Phase split — `<COLLAB | IRON> → <GOAL | AUTOPILOT>`

After rule 4 or 5 picks COLLAB or IRON, check whether the work divides into
an ordered judgment phase and a mechanical remainder. Emit a two-phase
verdict when **all** hold:

- phase 1 (COLLAB or IRON, chosen by rules 4–5 applied to the judgment part
  alone) settles every open decision the remainder depends on
- the remainder, judged as if phase 1 had already merged, meets rule 1
  (AUTOPILOT) or rule 3 (GOAL) on its own, prerequisites included
- the remainder is a substantial share of the work — roughly half the effort
  or more — so pricing the whole issue at phase 1's cost would overspend

Otherwise the verdict stays single. There are never more than two phases, and
phase 2 is always GOAL or AUTOPILOT. The task estimate counts both phases
together, and the SPLIT ceiling applies to that sum. Acceptance criteria no
gate can check (a native-speaker review, a go-live sign-off) belong to neither
phase: list them under *Why* as human follow-ups rather than letting them
block the split.

## Step 5 — Output (recommend, then confirm)

Print exactly this shape:

```
Issue #<number>: <title>
Verdict: <SPLIT | AUTOPILOT | DIRECT | GOAL | COLLAB | IRON> or <COLLAB | IRON> → <GOAL | AUTOPILOT>
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

For a two-phase verdict, insert before Recommended path:

Phases:
  1. <COLLAB | IRON> — <the decisions or work phase 1 settles>
  2. <GOAL | AUTOPILOT> — <the remainder>

and number the recommended path:

Recommended path:
  1. <phase 1 command>
  2. (after phase 1 merges) <phase 2 command>

For a `SPLIT` verdict, insert before the confirmation line:

Child issues:
  1. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>
  2. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>
  ...

Proceed with this path? [y/N]
```

The confirmation line depends on the verdict:

- SPLIT, DIRECT, COLLAB, IRON: `Proceed with this path? [y/N]`
- a two-phase verdict: `Proceed with phase 1? [y/N]`
- GOAL or AUTOPILOT alone: no question. End with `The router never launches GOAL or AUTOPILOT — run the command above yourself.`

Then **wait for the user**. On an explicit yes, invoke the recommended path.
On a yes to a two-phase verdict, invoke phase 1 only; the phase-2 command is
for the user to run after phase 1's PR merges, and its prerequisites should
be re-read then. The router never launches GOAL or AUTOPILOT, at any phase.
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

The verdict is platform-agnostic; the "recommended path" and how it is
invoked differ per host.

| Verdict | Claude | Codex |
|---|---|---|
| AUTOPILOT | nothing to invoke; print, for the user to run, in order: (1) `gh issue edit <number> --repo <owner>/<repo> --add-label agent:ready --add-label risk:<expected class>` — applying these labels is the user's authorization, and the router never applies them; (2) `ironmem autopilot lead --repo <owner>/<repo>=<checkout>`, which dispatches from `agent:ready`; (3) `ironmem autopilot advance --repo <owner>/<repo>=<checkout> --reviewer codex` (adding `--merge` executes the merge that is otherwise rehearsed) | the same commands, printed for the user |
| DIRECT | `/plan` to scope, then the `iron-tdd` skill | the `iron-tdd` skill directly |
| GOAL | print, for the user to run from a fresh worktree on a feature branch: `claude -p '/goal <condition> or stop after <N> turns'`, then open the PR yourself; a human reviews and merges it. | the same command, for the user to run in a terminal — `/goal` is a Claude Code command — then open the PR yourself; a human reviews and merges it. |
| IRON | invoke the `iron-plan` skill on the issue spec (flows into `iron-build`) | invoke the `iron-plan` skill |
| COLLAB | `/collab start <one-line task summary derived from the issue>` | `/collab` is Claude-driven: recommend the user run `/collab start <task>` in a Claude terminal (Codex joins via the protocol) |
| SPLIT | create the confirmed child issues, then re-run `/evaluate-issue` for each child | create the confirmed child issues, then re-run `/evaluate-issue` for each child |

For the COLLAB task summary, derive a single imperative line from the issue
title/body (e.g. `add schema migration 012 for code_map TTL`). Do not paste
the whole issue body into the command.

For the GOAL condition, write
`<one-line imperative task summary>, for issue #<number>. Done when this gate passes: <gate_commands joined with " && ">, and every change is committed and pushed on this branch`,
using the `gate_commands` read in Step 2. Default `<N>` to `6`, Autopilot's
own `DEFAULT_N_TURNS`. Keep the whole condition under 4,000 characters (the
cutting budget Autopilot applies to its own conditions): summarize; never
paste the issue body. For a phase-2 GOAL, name what phase 1 decided (its PR
or approved plan) in the summary. The argument is single-quoted, so write any
`'` inside it as `'\''`.

The `risk:<expected class>` label is what Autopilot merges under: decide_merge holds the PR unless it equals the class the reviewer derives from the diff.

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
Deep, regardless of the execution-path verdict. An AUTOPILOT verdict gets its
fresh-context reviewer from Autopilot itself; name `Autopilot's own reviewer`
as its review.

## Invariants

- **Read-only until the confirm gate.** Steps 1–4 never mutate state; the
  Step 2 prerequisite reads only read. The only action is invoking the
  chosen attended path *after* the user confirms.
- **Never auto-launch.** Even an obvious DIRECT verdict waits for `y`.
- **The router never launches GOAL or AUTOPILOT.** It prints their commands
  and the user runs them. It never labels an issue, creates labels, onboards
  or approves a repo, or dispatches — not even after a yes.
- **Task-budget first.** An estimate above 15 tasks always yields `SPLIT`;
  never launch collab for the oversized parent issue.
- **One verdict, never a menu.** A verdict is a single route or one ordered
  two-phase sequence (`<COLLAB | IRON> → <GOAL | AUTOPILOT>`) in which both
  phases run, in order. Never offer alternatives to choose between; the user
  overrides by declining. This replaces the earlier rule "Recommend one path,
  not a menu", which forbade phases as written: a phase sequence is not
  hedging, because neither phase is optional.
- **Adapt to Autopilot's policy, never relax it.** AUTOPILOT's envelope is
  `RiskClass::is_low_risk` in `crates/ironmem/src/autopilot/review.rs`; the
  collab template lint fails when this text drifts from it.
- **Scan, don't trace.** Step 2 is a handful of grep/glob calls and three
  prerequisite reads, not a subagent fan-out. If accurate routing would need
  deep tracing, say so in the rationale and lean toward the more cautious
  tier (usually COLLAB).
