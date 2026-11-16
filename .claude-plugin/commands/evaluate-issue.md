---
description: Read a GitHub issue, score its complexity, and recommend AUTOPILOT, DIRECT (/plan + iron-tdd), GOAL (bounded /goal loop), COLLAB (/collab start), IRON (iron-plan → iron-build), a two-phase COLLAB/IRON → GOAL/AUTOPILOT verdict, or mandatory SPLIT above 15 tasks. Advisory: prints a verdict and the exact command, then confirms before invoking; never launches GOAL or AUTOPILOT. Usage — /evaluate-issue <issue-number | #number | issue-url>
argument-hint: <issue-number | #number | issue-url>
---

<!-- DERIVED FROM docs/EVALUATE_ISSUE.md — protocol changes must update:
     - docs/EVALUATE_ISSUE.md (spec)
     - .claude-plugin/commands/evaluate-issue.md (this file)
     - .codex-plugin/prompts/evaluate-issue.md (Codex mirror) -->

You are the IronRace issue router. Full spec: `docs/EVALUATE_ISSUE.md`. The
user invoked `/evaluate-issue` with:

$ARGUMENTS

Read the issue, estimate its blast radius from a light repo scan, read the
unattended-route prerequisites, score six signals, and recommend exactly one
verdict. This is **advisory** — you print a verdict and the recommended
command, then wait for the user to confirm before invoking anything. Steps
1–4 are read-only.

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
     lists every `risk:*` label; rule 1 (AUTOPILOT) checks that the list
     contains `risk:<expected class>` for the class chosen in Step 3. Autopilot
     takes the merge class from the issue's `risk:*` label, so without it the
     PR is held.

A prose-only issue (docs, process, a question) may find nothing — that is
itself a DIRECT signal (AUTOPILOT in a repo meeting rule 1's prerequisites).

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
   three `agent:*` labels, and the `risk:<expected class>` label (Step 2.4),
   and the issue (Step 1 labels) carries no `risk:*` label other than
   `risk:<expected class>`; the expected diff is inside the
   auto-merge envelope; an implementer with only the issue body can finish it
   (no open human question, no criterion a gate cannot check); one branch,
   one PR. AUTOPILOT auto-merges only these classes: `documentation`, `dependency_bump`, `mechanical_rename`, `test_only`.
   Never recommend AUTOPILOT when the expected class is `logic`, `protocol`, `security`, or `public_api` —
   `decide_merge` holds those for a human regardless of reviewer PASS
   (`HoldForHuman(HighRiskClass)`, `crates/ironmem/src/autopilot/review.rs`).
   Name the class; when unsure, it is `logic`. If the issue otherwise fits
   but a prerequisite is missing, add `Missing AUTOPILOT prerequisite:` to
   *Why* with its fix, then continue down the list. No approved gate config
   → `ironmem autopilot onboard <owner>/<repo> --path <checkout>` then
   `ironmem autopilot approve <owner>/<repo>`; agent labels missing →
   `ironmem autopilot labels <owner>/<repo>`; `risk:<expected class>` label
   missing → `gh label create "risk:<expected class>" --repo <owner>/<repo>`;
   issue carries `risk:<other>` → name it; removing it is a human's risk
   call, never a printed command. Autopilot merges under the issue's `risk:*`
   label: `decide_merge` holds the PR unless that label's class equals the
   class the reviewer derives from the diff, and the gate and the review must
   also pass.

2. **DIRECT** — choose when **all** hold: ≤ ~2 files / single crate / module;
   one unit of work (1 task, or 2 steps too coupled to ship apart); no
   architectural / contract / migration / new-subsystem decision;
   well-specified. (Localized bug fix, small helper, doc/config fix, contained
   refactor, single added test.)

3. **GOAL** — choose when **all** hold: approved gate config (Step 2.4);
   "done" is fully expressible as that gate passing with the work committed
   and pushed; the work is mechanical or high-volume (many same-shape files,
   data or terminology authoring, fixtures, wiring a decided design) rather
   than judgment-shaped; no open design decision. It covers unattended work
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
   phases; phase 2 is always GOAL or AUTOPILOT. A phase-2 AUTOPILOT runs on
   a remainder issue of its own, which the user opens after phase 1 merges so
   that it can cite phase 1's PR. At phase-1 time the router only drafts that
   issue (the AUTOPILOT row below), and the remainder scope it drafts must
   pass rule 1's "implementer holding only the issue body" test by itself.
   The user then runs `/evaluate-issue <remainder number>` on the new issue;
   that run applies rule 1 to it and prints its label, `lead` and `advance`
   commands. A phase-2 GOAL needs no new issue: its condition follows the
   merged phase-1 decisions. The task estimate and the
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
  <exact command from the table below>

Recommended review:
  <review command — see review tier table below>

For a two-phase verdict, insert before Recommended path:

Phases:
  1. <COLLAB | IRON> — <the decisions or work phase 1 settles>
  2. <GOAL | AUTOPILOT> — <the remainder>

and number the path: `1. <phase 1 command>`, `2. (after phase 1 merges) <phase 2 command>`; number the review the same way: `1. <phase 1 tier review>`, `2. <phase 2 review — Autopilot's own reviewer for AUTOPILOT, the tier review for GOAL>`.

For `SPLIT`, insert before the confirmation line:

Child issues:
  1. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>
  2. <title> — <scope, acceptance summary, 1–15 task estimate, dependencies>
  ...

Proceed with this path? [y/N]
```

Confirmation line: `Proceed with this path? [y/N]` for SPLIT, DIRECT, COLLAB,
IRON; `Proceed with phase 1? [y/N]` for a two-phase verdict; for GOAL or
AUTOPILOT alone, no question — end with `The router never launches GOAL or AUTOPILOT — run the command(s) above yourself, in order.`

For GOAL or AUTOPILOT alone, stop after printing; otherwise **wait for the
user**. On an explicit yes, invoke the recommended path; for a two-phase
verdict, phase 1 only — phase 2 is the user's to run after phase 1's PR
merges. A phase-2 GOAL is printed as a ready-to-run command: before running
it, the user asks an agent session to run the Step 2 `get_drawer` read (no
read-only CLI prints it) and confirms that the gate config is still approved
and that its `gate_commands` still match the printed condition. Rerunning
`/evaluate-issue` on the parent is not the mechanism; it would re-score the
original issue and print the same two-phase verdict. A
phase-2 AUTOPILOT is printed as an instruction: the user opens the remainder
issue, then runs `/evaluate-issue <remainder number>` on it. The router never
launches GOAL or AUTOPILOT, at any phase, and never opens the remainder issue.
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

### Claude path mapping

| Verdict | What to do (on confirm, where there is one) |
|---|---|
| AUTOPILOT | Nothing to invoke. For an AUTOPILOT verdict, print for the user, in order: (1) `gh issue edit <number> --repo <owner>/<repo> --add-label agent:ready --add-label "risk:<expected class>"` — applying these labels is the user's authorization, and the router never applies them; (2) `ironmem autopilot lead --repo <owner>/<repo>=<checkout>`, which dispatches the next `agent:ready` issue — in-flight resumes first, then by `priority:*`, one per tick by default; each rerun dispatches whichever issue is next, at a dispatch's cost; (3) `ironmem autopilot advance --repo <owner>/<repo>=<checkout> --reviewer codex` once that dispatch has opened its PR (adding `--merge` executes the merge that is otherwise rehearsed). For a phase-2 AUTOPILOT, print none of these; phase 2's line (`2. (after phase 1 merges) …`) is an instruction in their place: open a remainder issue scoped to phase 2 that cites phase 1's PR — the remainder-issue command below, with `<remainder title>` and the remainder scope filled in and the `<phase-1 PR>` slot left — then run `/evaluate-issue <remainder number>` on it. The label, `lead` and `advance` commands come from that run, against `<remainder number>`. |
| DIRECT | Run `/plan` to scope, then invoke the `iron-tdd` skill. |
| GOAL | Nothing to invoke. Print for the user to run from a fresh worktree on a feature branch cut from the default branch (for a phase-2 GOAL, cut after phase 1 merges): `claude -p '/goal <condition> or stop after <N> turns'`. When the loop ends, the user opens the PR, and a human reviews and merges it. |
| IRON | Invoke the `iron-plan` skill on the issue spec; it flows into `iron-build`. |
| COLLAB | Run `/collab start <one-line imperative task summary derived from the issue>`. Do not paste the whole issue body. |
| SPLIT | Create the confirmed child issues, then run `/evaluate-issue` for each child. |

GOAL condition: `<one-line imperative task summary>, for issue #<number>. Done when this gate passes: <gate_commands joined with " && ">, and every change is committed and pushed on this branch`.
`<N>` defaults to `6` (Autopilot's `DEFAULT_N_TURNS`); keep the condition
under 4,000 characters (the `/goal` platform limit, covering the whole
argument after `/goal ` including `or stop after <N> turns`; a longer one is
refused and gets no turn; see `MAX_CONDITION_CHARS` in
`crates/ironmem/src/autopilot/turn_prompt.rs`) and never paste the issue body;
for a phase-2 GOAL, phase 1 has not run when the command is printed, so name
the decisions phase 1 settles and tell the loop to follow the merged phase-1
decisions on them, never a PR number; write any `'` as `'\''`.

A headless `claude -p` loop can edit, run the gate, commit and push only if
the flags it runs with permit it. The user grants that by appending one or
both of `--permission-mode <mode>` (choices in `claude --help`) and
`--allowed-tools`, after the quoted `/goal` argument. Together they must cover
file edits and writes, every `gate_commands` entry, and git; give
`--allowed-tools` one quoted, comma-separated value, e.g.
`--allowed-tools "Edit,Write,Bash(git *),Bash(<each gate command>)"`, or
`--permission-mode acceptEdits --allowed-tools "Bash(git *),Bash(<each gate command>)"`.
`acceptEdits` alone is never enough: it covers file edits, but not the gate
commands, `git commit` or `git push`, which go through Bash. If a gate command
contains `"`, `$` or a backtick, single-quote the value instead and write any
`'` in it as `'\''`. `--allowed-tools <tools...>` takes a variable number of
values, so its position after the quoted argument, not the comma, keeps it
from swallowing that argument as a tool name. The router never chooses a
permission mode or a tool list.

Phase-2 AUTOPILOT remainder-issue command:

```
gh issue create --repo <owner>/<repo> --title '<remainder title>' --body-file - <<'EOF'
<remainder scope, complete enough that an implementer holding only this body can finish it>

Remainder of #<number>, after phase 1's PR #<phase-1 PR>.
EOF
```

The title is single-quoted: write any `'` in it as `'\''`. The quoted heredoc
delimiter (`<<'EOF'`) stops command substitution and `$` expansion in the
body; no body line may be exactly `EOF`. Never put scope text in a
double-quoted argument. `<phase-1 PR>` stays a slot for the user to fill in
once phase 1's PR exists. `gh issue create` prints the new issue's URL, whose
number is `<remainder number>`. The router prints this command and never runs
it.

Print this block after the rest of the verdict output and just before the
confirmation line, in its own code fence, unindented, with `EOF` alone at
column 0: an indented `  EOF` never closes a `<<'EOF'` heredoc, and the
user's shell hangs waiting for it. Directly above the fence, print this one
header line:
`Run after phase 1 merges; replace <phase-1 PR> first:`
A yes to phase 1 never runs the block. Phase 2's numbered line points to this
block rather than holding it.

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
  for GOAL and AUTOPILOT it never adds or removes an issue's labels or opens
  a phase-2 remainder issue, and on no route does it create labels, onboard,
  approve, or start an Autopilot dispatch, even after a yes.
- **Task-budget first** — an estimate above 15 tasks always yields `SPLIT`;
  never launch collab for the oversized parent issue.
- **One verdict, never a menu.** A single route or one ordered two-phase
  sequence in which both phases run; never alternatives. The user overrides
  by declining.
- **Adapt to Autopilot's policy, never relax it** — the envelope is
  `RiskClass::is_low_risk` in `review.rs`, and the test
  `the_router_states_exactly_the_autopilot_envelope_is_low_risk_grants` in
  `review.rs` fails when this text drifts from it.
- **Scan, don't trace** — Step 2 is a few grep/glob calls plus three
  prerequisite reads. If accurate routing would need deep tracing, say so and
  lean to the more cautious tier.
