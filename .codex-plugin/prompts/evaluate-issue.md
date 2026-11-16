---
description: Read a GitHub issue, score its complexity, and recommend AUTOPILOT, DIRECT (iron-tdd), GOAL (bounded /goal loop), COLLAB (Claude-driven /collab start), IRON (iron-plan → iron-build), a two-phase COLLAB/IRON → GOAL/AUTOPILOT verdict, or mandatory SPLIT above 15 tasks. Advisory: prints a verdict and the exact next step, then confirms before invoking; never launches GOAL or AUTOPILOT. Usage — /evaluate-issue <issue-number | #number | issue-url>
---

<!-- DERIVED FROM docs/EVALUATE_ISSUE.md — protocol changes must update all
three in lockstep: docs/EVALUATE_ISSUE.md, .claude-plugin/commands/evaluate-issue.md,
and this file (.codex-plugin/prompts/evaluate-issue.md). -->

You are the IronRace issue router, running as **Codex**. Full spec:
`docs/EVALUATE_ISSUE.md`. The user invoked `/evaluate-issue` with:

$ARGUMENTS

Read the issue, estimate its blast radius from a light repo scan, read the
unattended-route prerequisites, score six signals, and recommend exactly one
verdict. This is **advisory** — print a verdict and the recommended next
step, then wait for the user to confirm before invoking anything. Steps 1–4
are read-only.

> Codex tool mapping: use your native shell for `git`/`gh`/`grep`, your file
> search for glob, and load skills natively from `~/.codex/skills`. Interpret
> any "invoke the <name> skill" instruction as activating that skill. Call
> ironmem MCP tools (e.g. `get_drawer`) through your MCP connection.

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
4. Read the unattended-route prerequisites for the **issue's** repo. All are
   reads; none creates, labels, or onboards anything:
   - **Approved gate config** — ironmem MCP
     `get_drawer(wing="autopilot", room="backlog-lineage", logical_key="gate-config:<slug>")`,
     `<slug>` = `<owner>/<repo>` with every character other than `A-Z`,
     `a-z`, `0-9`, `-`, `_`, `.` replaced by `-`. Approved only when found with
     JSON `state` `"approved"`; keep its `gate_commands`. Not found,
     `"pending"`, or the tool unavailable all mean not approved.
   - **Agent labels** —
     `gh label list --repo <owner>/<repo> --search "agent:" --limit 100 --json name -q '.[].name'`
     must list all three of `agent:ready`, `agent:blocked`, `agent:exhausted`.
   - **Risk label** —
     `gh label list --repo <owner>/<repo> --search "risk:" --limit 100 --json name -q '.[].name'`
     must list `risk:<expected class>`. Autopilot takes the merge class from
     the issue's `risk:*` label, so without it the PR is held.

A prose-only issue (docs, process, a question) may find nothing — that is
itself a DIRECT signal (AUTOPILOT in an onboarded repo).

## Step 3 — Score six signals

- **Blast radius** — files / crates / modules changed
- **Design judgment** — architectural decisions (API/contract, state
  machine, migration, new subsystem)?
- **Decomposability** — number of *independent* execution tasks; an estimate
  above 15 requires `SPLIT`
- **Spec clarity** — well-specified vs ambiguous / under-defined
- **Unattended fit** — could the repo's gate alone prove the work done? Which
  Autopilot risk class would the diff carry (`documentation`,
  `dependency_bump`, `mechanical_rename`, `test_only`, `logic`, `protocol`,
  `security`, `public_api`)? Drives AUTOPILOT, GOAL, and the phase split.
- **Security / review depth** — does it touch security-sensitive code (auth,
  credentials, billing, user data)? Drives the post-implementation review
  tier, not the execution path.

## Step 4 — Decide (first match wins)

Check in this order — the hard `SPLIT` task-budget gate comes first;
AUTOPILOT next, since an eligible dispatch needs no human attached; DIRECT is
the cheapest attended route; GOAL is the unattended loop for judgment-free
work too large for DIRECT; COLLAB is the most expensive and must be
justified; IRON is the default middle. The counts below touch at the seams;
when they overlap, the deciding factor is **task independence, not the
number** (one tightly-coupled unit → DIRECT; two or more independently
shippable tasks → IRON), and COLLAB's
design-judgment triggers dominate its crate-count range (a purely mechanical
3+-crate rename is IRON, not COLLAB). Security sensitivity alone does
not justify COLLAB — it drives the review tier recommendation instead.

0. **SPLIT** — choose before every other route when the issue credibly needs
   **more than 15 independent execution tasks**. Do not start collab for the
   parent. Propose 2+ independently executable child issues, each with focused
   scope, acceptance criteria, dependencies, and a 1–15-task estimate. Never
   merge unrelated work, weaken acceptance criteria, or discard scope merely to
   fit the limit. Keep the parent open as the tracking issue.

1. **AUTOPILOT** — choose when **all** hold: approved gate config, all
   three `agent:*` labels, and the `risk:<expected class>` label (Step 2.4);
   the expected diff is inside the
   auto-merge envelope; an implementer with only the issue body can finish it
   (no open human question, no criterion a gate cannot check); one branch,
   one PR. AUTOPILOT auto-merges only these classes: `documentation`, `dependency_bump`, `mechanical_rename`, `test_only`.
   Never recommend AUTOPILOT when the expected class is `logic`, `protocol`, `security`, or `public_api` —
   `decide_merge` holds those for a human regardless of reviewer PASS
   (`HoldForHuman(HighRiskClass)`, `crates/ironmem/src/autopilot/review.rs`).
   Name the class; when unsure, it is `logic`. If the issue otherwise fits
   but a prerequisite is missing, add `Missing AUTOPILOT prerequisite:` to
   *Why* with its fix (`ironmem autopilot onboard <owner>/<repo> --path <checkout>`
   then `ironmem autopilot approve <owner>/<repo>`;
   `ironmem autopilot labels <owner>/<repo>`; or
   `gh label create "risk:<expected class>" --repo <owner>/<repo>`) and
   continue down the list.

2. **DIRECT** — choose when **all** hold: ≤ ~2 files / single crate / module;
   one unit of work (1 task, or 2 steps too coupled to ship apart); no
   architectural / contract / migration / new-subsystem decision;
   well-specified. (Localized bug fix, small helper, doc/config fix, contained
   refactor, single added test.)

3. **GOAL** — choose when **all** hold: approved gate config (Step 2.4);
   "done" is fully expressible as that gate passing with the work committed;
   the work is mechanical or high-volume (many same-shape files, data or
   terminology authoring, fixtures, wiring a decided design) rather than
   judgment-shaped; no open design decision. It covers unattended work
   outside the auto-merge envelope or in a repo without the labels; a human
   merges its PR. If it otherwise fits but the gate config is not approved,
   add `Missing GOAL prerequisite:` to *Why* with the onboard + approve
   commands and continue down the list.

4. **COLLAB** — choose when **any** hold: requires real design judgment
   (public API/contract, state-machine, schema migration, new subsystem);
   ambiguous requirements that benefit from a second independent perspective;
   large blast radius (~3+ crates or many interacting modules) with tightly
   interacting changes. Security sensitivity alone does not justify COLLAB —
   it is handled by the review recommendation. (Protocol/state-machine
   changes, migrations, cross-crate features.)

5. **IRON** — the default middle when nothing above fits: 2–6
   independently shippable tasks; moderate blast radius (1–2 crates);
   plannable up front; no cross-model design review needed. (A feature within
   an existing subsystem, a multi-file mechanical change in a repo with no
   approved gate config, test-coverage expansion.)

6. **Phase split** — after COLLAB or IRON is picked, emit
   `<COLLAB | IRON> → <GOAL | AUTOPILOT>` when **all** hold: phase 1 settles
   every decision the remainder depends on; the remainder, judged as if phase
   1 had merged, meets rule 1 or rule 3 on its own, prerequisites included;
   the remainder is roughly half the effort or more. Never more than two
   phases; phase 2 is always GOAL or AUTOPILOT; the task estimate and the
   SPLIT ceiling cover both phases together. Criteria no gate can check
   (native-speaker review, go-live sign-off) belong to neither phase — list
   them as human follow-ups.

When one signal pulls toward COLLAB but the rest sit firmly in IRON,
name the tension in the rationale rather than silently rounding up.

## Step 5 — Output, then confirm

Print exactly this shape:

```
Issue #<number>: <title>
Verdict: <SPLIT | AUTOPILOT | DIRECT | GOAL | COLLAB | IRON> or <COLLAB | IRON> → <GOAL | AUTOPILOT>
Task estimate: <N | N+> independent execution tasks

Why:
 - <signal>: <one-line evidence>
 - <signal>: <one-line evidence>

Blast radius: <N files, M crates> (<main ones>)

Recommended path:
  <exact next step from the table below>

Recommended review:
  <review command — see review tier table below>

For a two-phase verdict, insert before Recommended path:

Phases:
  1. <COLLAB | IRON> — <the decisions or work phase 1 settles>
  2. <GOAL | AUTOPILOT> — <the remainder>

and number the path: `1. <phase 1 step>`, `2. (after phase 1 merges) <phase 2 command>`.

For `SPLIT`, insert before the confirmation line:

Child issues:
  1. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>
  2. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>
  ...

Proceed with this path? [y/N]
```

Confirmation line: `Proceed with this path? [y/N]` for SPLIT, DIRECT, COLLAB,
IRON; `Proceed with phase 1? [y/N]` for a two-phase verdict; for GOAL or
AUTOPILOT alone, no question — end with `The router never launches GOAL or AUTOPILOT — run the command above yourself.`

Then **wait for the user**. On an explicit yes, invoke the recommended path;
for a two-phase verdict, phase 1 only — the user runs phase 2 after phase 1
merges, re-reading its prerequisites then.
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
| AUTOPILOT | Nothing to invoke. Print for the user, in order: (1) `gh issue edit <number> --repo <owner>/<repo> --add-label agent:ready --add-label risk:<expected class>` — applying these labels is the user's authorization, and the router never applies them; (2) `ironmem autopilot lead --repo <owner>/<repo>=<checkout>`, which dispatches from `agent:ready`; (3) `ironmem autopilot advance --repo <owner>/<repo>=<checkout> --reviewer codex` (adding `--merge` executes the merge that is otherwise rehearsed). |
| DIRECT | Invoke the `iron-tdd` skill directly. |
| GOAL | Nothing to invoke. `/goal` is a Claude Code command: print for the user to run in a terminal, from a fresh worktree on a feature branch: `claude -p '/goal <condition> or stop after <N> turns'`, then open the PR yourself; a human reviews and merges it. |
| IRON | Invoke the `iron-plan` skill on the issue spec; it flows into `iron-build`. |
| COLLAB | `/collab` is Claude-driven. Recommend the user run `/collab start <one-line imperative task summary derived from the issue>` in a Claude terminal; Codex joins via the protocol. Do not paste the whole issue body. |
| SPLIT | Create the confirmed child issues, then run `/evaluate-issue` for each child. |

GOAL condition: `<one-line imperative task summary>, for issue #<number>. Done when this gate passes: <gate_commands joined with " && ">, and every change is committed and pushed on this branch`.
`<N>` defaults to `6` (Autopilot's `DEFAULT_N_TURNS`); keep the condition
under 4,000 characters and never paste the issue body; for a phase-2 GOAL,
name what phase 1 decided; write any `'` as `'\''`.
The `risk:<expected class>` label is what Autopilot merges under: decide_merge holds the PR unless it equals the class the reviewer derives from the diff.

### Review tiers

Based on change complexity and security sensitivity, recommend one
post-implementation review. This is independent of the execution-path verdict
— a DIRECT fix in auth code still gets a Deep review.

| Tier | When | Command |
|---|---|---|
| Standard | Small, contained changes with no security surface | `/code-review` |
| Thorough | Multi-file changes, moderate complexity | `/pr-review-toolkit:review-pr` |
| Deep | Complex changes, security-sensitive code (auth, credentials, billing, user data), or high blast radius | `/ultrareview-local` |

Security-sensitive changes always warrant at minimum Thorough, and typically
Deep, regardless of the execution-path verdict. AUTOPILOT's review is
`Autopilot's own reviewer`.

## Invariants

- **Read-only until the confirm gate.** Steps 1–4 never mutate state; the
  prerequisite reads only read.
- **Never auto-launch** — even an obvious DIRECT waits for `y`.
- **The router never launches GOAL or AUTOPILOT** — it prints their commands;
  it never labels, creates labels, onboards, approves, or dispatches, even
  after a yes.
- **Task-budget first** — an estimate above 15 tasks always yields `SPLIT`;
  never launch collab for the oversized parent issue.
- **One verdict, never a menu.** A single route or one ordered two-phase
  sequence in which both phases run; never alternatives. The user overrides
  by declining.
- **Adapt to Autopilot's policy, never relax it** — the envelope is
  `RiskClass::is_low_risk` in `review.rs`, and the lint pins this text to it.
- **Scan, don't trace** — Step 2 is a few grep/glob calls plus three
  prerequisite reads. If accurate routing would need deep tracing, say so and
  lean to the more cautious tier.
