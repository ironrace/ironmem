# Autopilot — Decomposition, Work Shape, and Merge Authority

**Date:** 2026-09-12
**Scope:** Amends `2026-08-21-autonomous-backlog-runner-design.md` (rev 11). Changes **what the IC is dispatched against** (a plan, not an issue), adds a **second classification axis**, makes **feature flags a first-class autonomy mechanism**, and resolves the **merge-authority** question that spec left open at rung 6. Does not touch `collab`, `iron-spec`, or the HumanLayer epic.
**Status:** **DRAFT — not approved.** Written 2026-09-12 after Autopilot's first end-to-end live run. Requires Jeff's approval before any rung is built.
**Revision:** rev 1.

> This document exists because the first live run succeeded at everything except the one thing that makes the subsystem autonomous.

---

## Problem

On 2026-09-12/13 Autopilot ran end to end for the first time, on issue #339 → PR #343, for **$7.05** of a $25 daily ceiling. Every leg of the spec's data flow executed live **except the merge**:

| leg | result |
|---|---|
| Lead selects `agent:ready`, dispatches IC | ran |
| IC edits, gates, pushes | `Met` @ `2df03ad` |
| `advance` opens the PR | **PR #343 → main** |
| fresh-context Codex review | ran ×3, all `NeedsChanges` |
| `decide_merge` | `HoldForHuman`, correctly |
| remediation armed + dispatched (rung 11) | ran ×2 |
| `retry` grant recovery | ran |
| **merge** | **never — no PASS was ever reached** |

Three problems surfaced, and they are not independent.

**1. The outer loop has no terminating condition it can reach.** Three review rounds produced **3, then 4, then 2 findings — nine in total, every one verified true at source, none repeated.** The reviewer was doing real work and the document measurably improved. It still did not converge, and every round closed with the same sentence:

> *No tests changed; the green gate does not validate these prose claims.*

**2. The unit of work is too large.** #339 was a single 224-line document dispatched as one issue. Nine distinct defects in one review surface is a symptom of the unit, not of the IC.

**3. Nothing can merge.** `decide_merge` has never returned an executed merge, because it has never been handed a PASS — and even with one, `main` requires a human approving review with `enforce_admins: true`, and there is no second identity able to give it.

---

## What is already decided, and therefore not re-litigated here

Two of the parent spec's approved goals settle questions that would otherwise look open:

- **Goal 4** — *"The human approves **envelopes** … never individual steps."*
- **Goal 5** — *"No change reaches the default branch without either a human **or an independent fresh-context reviewer** having read the diff. Deterministic gates alone are never sufficient authority to merge."*

**Goal 5 already grants merge authority to a reviewer PASS.** A bot approval implementing that is *executing an approved decision*, not widening one. What blocks it today is GitHub branch protection, which is configuration, not design. `advance.rs:476` states the same intent from the other end:

> *Requiring a human to have written `risk:documentation` before that can happen is not a gap in the automation; it is the authorization.*

---

## The two-loop model

Autopilot is often described as "`/goal` plus gates." That is exactly right for one of its two loops and misleading for the other.

```
inner loop   IC dispatch        condition: gate green AND pushed     bound: N turns
outer loop   remediation        condition: reviewer PASS             bound: attempt cap
```

The inner loop terminates because **the gate is a predicate**: re-run it on the same commit and it returns the same answer. The outer loop's condition is **a judgment**. A reviewer is not a pure function of the SHA — it can find new things on unchanged code.

Autopilot currently *treats* it as a predicate: `advance` prints `already reviewed` and reuses the stored verdict keyed by head SHA. That caching is correct for cost and wrong about what a review is.

**The generalisation that matters:** the reviewer has no oracle for *any* class. For code, the gate independently constrains what can change, so the reviewer's subjectivity has limited room. For documentation the gate constrains nothing, so the reviewer is the only constraint and is unbounded. Documentation is where it is visible, not where it lives.

> **Design rule.** The outer loop may run unbounded only where an objective gate independently constrains the result. Where it does not, convergence is the only available termination signal and must be measured rather than assumed.

---

## Decision 1 — The IC is dispatched against a plan, not an issue

**Today:** `run_issue` takes one issue and hands its whole body to one IC in one worktree producing one PR.

**Proposed:** an issue above a complexity threshold is decomposed into ordered units, each of which becomes its own dispatch, its own PR, and its own merge.

This is not new construction. It is connecting machinery that exists and has already worked here:

- `iron-plan` produces tier-tagged plans; `iron-build` dispatches a fresh subagent per task with review after each.
- `/evaluate-issue` already returns **SPLIT**, mandatory above 15 tasks.
- **#283 was split into #297 → #298 → #299 and all three merged.**
- **The Autopilot ladder itself was built this way** — eleven rungs, PRs #317–#329, each merging independently.

The strongest evidence that this works is that *this subsystem was built by the method it cannot yet use.*

**Sequential against `main`, not stacked.** Each unit is cut from `main` after the previous one merges. Stacking is deliberately excluded for now: PR #286's stacked split produced a standing rule — *check downstream branches before keeping an auto-fix* — because every change to PR 1 ripples through 2, 3, 4. An unattended loop has no judgment for resolving that ripple, and the failure mode strands the whole stack. Sequential is slower and terminates.

**Why it helps the outer loop:** a smaller unit presents a smaller review surface, so a PASS is reachable in fewer rounds. #339's nine findings across three rounds is the counterexample that motivated this.

---

## Decision 2 — A second axis: shape of work

The existing eight `risk:*` classes measure **risk if wrong**. That is the right axis for merge authorization and is unchanged.

Decomposability is a *different* property, and the two cut across each other:

| | typical risk class | typical size | decomposable? |
|---|---|---|---|
| **feature** | `logic` / `public_api` | large | **yes** — vertical slices |
| **refactor** | `mechanical_rename` | large | **poorly** — coordination is the point |
| **bug** | `logic` | small | usually moot |

A refactor is low-risk and large; a bug fix is high-risk and small. Collapsing this into the risk labels would corrupt the axis that merge authority depends on, so it must be recorded separately.

**Refactors are the hard case and are explicitly not solved here.** They resist decomposition because the coordinated change *is* the work, and they cannot hide behind a flag without maintaining both paths. See *Open questions*.

---

## Decision 3 — Feature flags change the risk class

A change behind a disabled flag **cannot execute**, and code that cannot execute cannot break anything.

This is the mechanism that lets *new feature work* enter the auto-merge envelope without widening it. The envelope is `documentation | dependency_bump | mechanical_rename | test_only`; everything else fails closed. Rather than granting Autopilot merge rights over `logic` — which would gut Goal 5's protection — new code lands disabled, repeatedly and safely, and a **human flips the flag** as the single reviewed decision.

That is Goal 4 exactly: the human approves an envelope, not each step.

**Precedent in this subsystem:** rung 9 shipped off by default behind `--advisor` and rung 11 behind `--remediate`; rung 10 shipped enabled but withheld its irreversible half behind `--merge`. The ladder already used flags to land risky capability incrementally — including the pattern of landing a capability *on* while keeping its one irreversible action *off*.

**Costs, stated plainly.** Flag debt is real and someone must remove dead flags. Flags do not help refactors. And a flag is only a risk reducer if the disabled path is genuinely inert — a flag checked in one place and bypassed in another is worse than none.

---

## Decision 4 — Merge authority

Making a PASS actually merge requires three things, in this order. **The first is worth doing whether or not autonomy is ever enabled.**

**1. Required status checks.** `main` currently reports:

```
approvals: 1   enforce_admins: true   dismiss_stale: true
codeowners: false   last_push_approval: false   required_checks: null
```

**`required_checks` is null.** Nothing at the branch level requires CI to pass; the sole gate is the human approval. Today that is masked because a human reads every PR. The moment anything else can approve, `main` has **no enforced gate at all**. The objective gate must exist before any approval automation does.

**2. Fix the `agent:blocked` trap (#346).** A merge hold parks the issue in `agent:blocked`; `advance` skips that label; and `agent:blocked` resumes on *a newer human comment*. **An approval is not a comment.** So today even a human approving does not resume a held PR. Until this is fixed, no PASS can reach a merge by any route.

**3. A second approving identity.** GitHub forbids self-approval and `jcagentszero` authors the PRs, so this must be a distinct identity — a GitHub App with `pull_requests:write` that posts an approving review **only** when `decide_merge` returns `WouldMerge`. This is not an admin bypass: it is an auditable review in the PR timeline, revocable by uninstalling the app, and it implements Goal 5 rather than circumventing it.

`dismiss_stale: true` works in favour of this: the approval must follow the last push, and rung 6 already refuses to merge unless the reviewed SHA is still the head. The two constraints agree.

**The tradeoff, stated so it is chosen rather than discovered.** This moves the human decision from *"approve this diff"* to *"label this issue"* — which happens **before the diff exists**. That is defensible only because the envelope is four low-risk classes and everything else fails closed. With Decision 3, the flag flip becomes a second human decision taken *after* the code exists, which restores diff-sighted judgment for feature work.

---

## Decision 5 — The review loop converges rather than counting

Replace the remediation round cap with a convergence test.

- **New findings each round → progress → continue.** (#339: nine findings, zero repeats — it was converging.)
- **Findings repeat or overlap → stalled → escalate to a human.**

This preserves what rung 11 was protecting. `remediate.rs:100` records the reasoning for the current bound:

> *the other way round, an armed record would shadow the cap forever and the human would never be told*

The concern is that the human is *told*, not that the number is small. A stall detector preserves that while removing an arbitrary ceiling. Rung 7/9 already has thrash detection over attempt signatures; this applies the same idea to review findings.

**Bound the loop in spend and wall-clock, not attempts.** And count reviews separately, because `review.rs:47` records that `codex exec` emits no price and has **no `--max-budget-usd` equivalent at all** — so the dollar ceiling provably cannot bind the reviewer. A ceiling denominated in units the thing never reports is not a ceiling.

**Prerequisite: #344.** The remediation currently shares the issue's attempt cap with the original work, so #339 got three rounds not by policy but because writing the doc consumed two of five. Any round policy is meaningless until a remediation has a budget of its own.

**Review depth should be routed, not uniform.** A deep multi-lens review (`/ultrareview-local`) is plausibly cheaper end-to-end than N cheap rounds plus N remediation dispatches — this run spent most of $7.05 on exactly that and still did not converge. But it must be routed by diff size and class, or every typo fix pays for ten lenses. Two cautions:

- **Depth is not accuracy.** Measured here: `ultrareview` ran ~75% false-positive on #273, while the single Codex reviewer went **9 for 9 true** on #343. Adversarial verification is the hypothesis that closes that gap; it is untested in this subsystem.
- **An auto-fixing reviewer is a second writer on the branch**, which rung 11 explicitly scoped out, and whose "all fixed" reports have been true-but-uncommitted eight times in this repo. An unattended loop cannot inspect the working tree.

---

## Consequences

- The IC's contract changes from *issue → PR* to *plan unit → PR*. `run_issue` and the lineage schema both assume one issue, one dispatch, one PR.
- Decomposition adds a planning step the loop does not have, and a wrong split is a new failure mode with no current recovery.
- Sequential merging means wall-clock scales with unit count. This is accepted in exchange for removing the stacked-ripple failure.
- Feature flags create a cleanup backlog that is nobody's job today.
- A second approving identity is a real credential with merge reach; its blast radius is bounded only by `decide_merge` failing closed.

---

## Migration

Ordered so each step is independently valuable and none depends on an unbuilt successor:

1. **Required status checks on `main`.** Valuable alone; prerequisite for everything in Decision 4.
2. **#346** — the `agent:blocked` trap. Required before any PASS can merge, by any route.
3. **#344 + #345** — remediation budget and exhaust close-out. Required before any round policy means anything.
4. **Convergence detector** (Decision 5), still under a spend/wall-clock ceiling.
5. **Decomposition** (Decision 1), sequential only.
6. **Work-shape axis** (Decision 2) and **flags** (Decision 3).
7. **Approving identity** (Decision 4.3) — last, and only once 1–3 hold.

---

## Open questions

1. **Where does decomposition live?** Inside Autopilot, or as a pre-step via `/evaluate-issue` → `iron-plan` (which already does it)? The latter reuses proven machinery; the former keeps Autopilot self-contained, which the parent spec's non-goals favour.
2. **How are refactors bounded?** They neither decompose cleanly nor hide behind flags. Do they stay human-routed indefinitely?
3. **Who removes dead flags?** If nobody, Decision 3 trades one backlog for another.
4. **Is deep review actually cheaper than N cheap rounds?** Measurable now, against ground truth: PR #343 has nine hand-verified findings. Run `/ultrareview-local` against it and compare.
5. **Does a reviewer PASS mean anything on a class with no oracle?** If `documentation` cannot reliably converge, it may belong outside the auto-merge envelope despite being low-risk — which would invert the envelope's current membership.
6. **When is stacking worth the ripple?** Sequential is the safe default; there may be a size at which it is too slow.

---

## Evidence log

- **First end-to-end live run**, 2026-09-12/13, issue #339 → PR #343, $7.05, 9 dispatches, 3 reviews (the two `--remediate` passes reused the stored verdict rather than re-reviewing). Every leg but the merge.
- **Review rounds:** 3 → 4 → 2 findings; nine distinct, zero repeated, all verified true at source.
- **Defects found by living through them:** #344, #345, #346.
- **The ladder's own history:** rungs 0–11 as PRs #317–#329, each merging independently; rungs 9/10/11 off by default behind flags.
- **Prior decomposition here:** #283 → #297 → #298 → #299, all merged. PR #286's stacked split produced the downstream-ripple rule.
- **Branch protection read** 2026-09-12: `required_checks: null`.
- **Reviewer cost:** `review.rs:47` — `codex exec` emits no price and has no per-invocation ceiling.
