# Autopilot — Decomposition, Work Shape, and Merge Authority

**Date:** 2026-09-12
**Scope:** Amends `2026-08-21-autonomous-backlog-runner-design.md` (rev 11). Changes **what the IC is dispatched against** (a sub-issue, not a whole issue), adds a **second classification axis**, proposes **feature flags as an autonomy mechanism**, and proposes **merge authority**, which rung 6 decided against. Does not touch `collab`, `iron-spec`, `iron-tdd`, or the HumanLayer epic. **Does touch `iron-build`** — see *What this reverses*.
**Status:** Draft
**Revision:** rev 2 — rewritten after a three-agent review of rev 1 found a self-contradiction in Decision 5, a circular trigger in Decision 4, and an overstated reading of the parent spec's Goal 5. Rev 1 is superseded, not amended; where it was wrong this document says so rather than quietly correcting.

> Architecture, Error handling and Validation log are deliberately deferred until approval. Testing and Data flow are **not** deferred, because rev 1's review showed both were load-bearing for decisions it made.

---

## What this reverses

Rev 1 claimed to change no approved decision. That was wrong. This document reopens three things, listed here so an approver sees the surface before the argument:

1. **Parent non-goal** (parent spec line 52): *"No changes to `collab`, `iron-build`, `iron-spec`, or `iron-tdd`. This is a sibling subsystem."* Decision 1 proposes Autopilot depend on `iron-plan`/`iron-build` decomposition machinery. Rev 1 silently dropped `iron-build` from its own scope line rather than admitting the reversal.
2. **Rung 6's merge-authority decision** (`merge.rs:59-63`): *"Autopilot is not the reviewer of record there and **cannot become one**."* Decision 4 proposes exactly that it become one. Rev 1 mischaracterised this as a question the parent spec "left open"; rung 6 **closed** it, in the opposite direction.
3. **Rung 11's bounded remediation** (`remediate.rs:103-104`). Decision 5 changes how the bound is computed — but, unlike rev 1, **keeps a bound that provably terminates.**

---

## Problem

On 2026-09-12/13 Autopilot ran end to end for the first time, on issue #339 → PR #343, for **$7.05** of a $25 daily ceiling. Nine dispatches across four Lead ticks (2 + 3 + 1 + 3, per the tick summaries) and three fresh reviews at three distinct head SHAs.

| leg | what happened |
|---|---|
| Lead selects `agent:ready`, dispatches IC | worked |
| IC edits, gates, pushes | worked — `Met` @ `2df03ad` |
| `advance` opens the PR | worked — PR #343 → main |
| fresh-context review | ran 3×, `NeedsChanges` each time |
| `decide_merge` | worked — `HoldForHuman`, correctly |
| remediation armed + dispatched | **ran 2×, but both arming passes reused a stored verdict rather than re-reviewing** |
| `retry` grant recovery | worked |
| **merge** | **never ran** |

Rev 1 summarised this as "succeeded at everything except the merge." That was generous framing of a run that also produced three defect reports (#344, #345, #346) and a document needing nine fixes. The table above separates *executed* from *worked* because rev 1's review showed the distinction was being elided.

**Three blockers stand between this run and a merge, and rev 1 named only the third.** (1) No PASS was ever reached. (2) Even with a PASS, #346 means a held issue parks in `agent:blocked`, which `advance` skips. (3) Only then does branch protection apply. Rev 1's claim that "what blocks it today is GitHub branch protection" was wrong, and contradicted by its own migration ordering.

Three underlying problems:

**1. The outer loop's bound was miscomputed, not misdesigned.** Three rounds produced 3, then 4, then 2 findings — nine distinct, none repeated, all verified. But #339 got exactly three rounds by accident: writing the document consumed 2 of its 5 *attempts*, and the remediation inherited the remaining 3 (#344). Attempts and review rounds are different units, and rev 1 conflated them.

**2. The unit of work is too large.** Nine distinct defects in one 224-line document is a property of the unit, not of the IC.

**3. Nothing can merge.** See the three blockers above.

---

## What the parent spec actually decided

Rev 1 asserted that Goal 5 "already grants merge authority to a reviewer PASS," and used that to place Decision 4 outside debate. **That reading is wrong and is withdrawn.**

Goal 5 verbatim (parent spec line 42):

> No change reaches the default branch without either a human or an independent fresh-context reviewer having read the diff. Deterministic gates alone are never sufficient authority to merge.

This is a **prohibition**. "Not without X" makes X *necessary*; it does not make X *sufficient*. The sentence uses "sufficient" exactly once, and only to deny it to gates. `review.rs:135-138` — *"Everything touching logic, protocol, security, or public API opens a PR and waits for a human **regardless of reviewer verdict**"* — is only coherent if a PASS was never sufficient on its own.

The real authorization is a **conjunction**: Goal 5's floor, **and** Goal 4's envelope, **and** the class gate, **and** the five further conditions `decide_merge` checks past a PASS (`review.rs:840-865`).

**Consequence for this document: Decision 4 is a new proposal and must be argued on its merits.** It cannot be justified by citing an approval that was never given.

---

## The two-loop model

Autopilot is often described as "`/goal` plus gates." That is exactly right for one of its two loops and misleading for the other.

```
inner loop   IC dispatch    condition: gate green AND pushed    bound: N turns
outer loop   remediation    condition: reviewer PASS            bound: attempt cap
```

The inner loop terminates because **the gate is a predicate**: re-run it on the same commit and it returns the same answer. The outer loop's condition is **a judgment**. A reviewer is not a pure function of the SHA — it can find new things on unchanged code.

> **Design rule.** The outer loop may run unbounded only where an objective gate independently constrains the result. Where it does not, no *judgment-derived* signal can supply a termination proof, and the bound must come from a counter.

This rule is the strongest reasoning available here, and **it is what refutes rev 1's own Decision 5** — which proposed replacing a counter with a judgment-derived signal. Decision 5 below is rewritten to obey it.

---

## Decision 1 — The IC is dispatched against a sub-issue

**Today:** `run_issue` takes one issue and hands its body — truncated past `MAX_CONDITION_CHARS` — to one IC in one worktree. `advance` later opens the PR.

**Proposed:** an issue above a complexity threshold is decomposed into ordered units, and **each unit becomes a real GitHub sub-issue** (`parent` / `sub-issues`, native and available on this repo — verified 2026-09-12).

**Why sub-issues rather than a new internal unit type.** Every per-issue mechanism then works unchanged: `agent:ready` and `risk:*` labels, lineage, the attempt cap, the `dispatch-state` drawer keyed `logical:dispatch-state:<repo>-<n>`, `queue::plan_queue`, and `advance`. **No schema change, no new key, no parallel bookkeeping.** This is the answer to rev 1's missing data-flow amendment: there isn't one to write, because the unit *is* an issue.

The parent issue closes when its sub-issues do.

**Sequential against `main`, not stacked.** Each unit is cut from `main` after the previous merges. PR #286's stacked split produced the standing rule *check downstream branches before keeping an auto-fix*; an unattended loop has no judgment for resolving that ripple, and the failure mode strands the whole stack.

**Evidence, stated with its limits.** `iron-plan` produces tier-tagged plans and `iron-build` dispatches a fresh subagent per task with review after each. `/evaluate-issue` already returns SPLIT, mandatory above 15 tasks. #283 was split into #297 → #298 → #299 and all three merged.

Rev 1 called the Autopilot ladder itself — eleven rungs as PRs #318-#320 and #322-#329, each merging independently — the "strongest evidence" that this works. **That overstated it**, in two ways rev 1's review identified and this document accepts:

- A **human** decomposed and sequenced those rungs while holding an approved spec, and a **human approving review** merged each one. The proposed method has a human at neither step. The ladder is evidence that *human-planned decomposition with human-reviewed merges* works here.
- #283's split is not the clean precedent rev 1 implied: **#299 discovered mid-flight that `claimable` ≠ dispatchable and had to add `tokenless_admitted`.** The split was revised during execution, by judgment an unattended loop does not have. Only successful splits are counted; no denominator is available.

(Rev 1 also wrote the range as "#317–#329," which includes #321, an unrelated MCP PR, and described eleven rungs as "rungs 0–11," which is twelve. #317 is the design spec.)

**A wrong split is a new failure mode with no recovery today.** That is the central risk of this decision and it is not solved here.

---

## Decision 2 — A second axis: shape of work

The eight `risk:*` classes measure **risk if wrong**. That is the right axis for merge authorization and is unchanged.

Decomposability is a different property, and the two cut across each other:

| | typical risk class | typical size | decomposable? |
|---|---|---|---|
| **feature** | `logic` / `public_api` | large | **yes** — vertical slices |
| **refactor** | `mechanical_rename` | large | **poorly** — coordination is the point |
| **bug** | `logic` | small | usually moot |

Collapsing this into the risk labels would corrupt the axis merge authority depends on, so it is recorded separately.

**Refactors are the hard case and are not solved here.** They resist decomposition because the coordinated change *is* the work, and they cannot hide behind a flag without maintaining both paths.

---

## Decision 3 — Feature flags, and what they do not do

Rev 1 claimed *"a change behind a disabled flag cannot execute, and code that cannot execute cannot break anything,"* and concluded that flags **change the risk class**. **That claim is withdrawn.** It is roughly true for a deployed service and substantially false for ironmem, which ships as a binary, a library and a plugin on users' machines. Rev 1's review enumerated the failure modes; they are recorded here because they are the decision:

- **A flag in shipped software is not a control surface.** A service flips a flag centrally and reverses it the same way. Here, flipping on is a *release*, and flipping back off is *another release every user must pull*. Rev 1's "the human flips the flag as the single reviewed decision" imported service-shaped reversibility that does not hold.
- **SQLite migrations run at startup regardless of the flag.** ironmem has on-disk state. A flagged feature's schema change is irreversible the first time the binary runs, flag off or not. **This alone defeats "inert."**
- **The disabled path still ships.** New `pub` items are public API the moment they land in a library crate — a `public_api` consequence arriving under a `documentation` label. New dependencies are fetched, built, linked, and carry supply-chain, licence and binary-size cost. `cargo clippy --workspace --all-targets --all-features` compiles all of it.
- **The edits that made room for the feature are not behind the flag** — threading a parameter, adding an enum variant, changing a signature. The flag guards new behavior, not the refactor that admitted it.
- **Startup-time registration is not runtime-gated**: clap subcommands (the `--help` surface), MCP tool registration (`tools/list` output every client sees), serde variants.
- **The disabled path is the least-tested code in the tree.** With the flag off in CI, the enabled branch is first exercised at flip time — the moment the human's single decision is spent.
- **Batching defeats the argument entirely.** If N units land disabled and one flip enables them all, that flip is one decision covering N unreviewed diffs, made with the context of none.

**What survives.** Flags remain valuable for *incremental landing* — which is how rungs 9 and 11 shipped (`--advisor`, `--remediate`) and how rung 10 shipped enabled with its irreversible half behind `--merge`. That precedent is real, but it is again **human-reviewed, human-merged** landing.

**What does not survive is using a flag to move work between risk classes.** Any such reclassification would require a *mechanical* inertness check — that the diff adds no enabled-path behavior, no migration, no public API, no dependency — and no such check exists. Until one does, **flags do not change the risk class**, and Decision 3 makes no claim on the auto-merge envelope.

---

## Decisions 3 and 4 composed

Rev 1 argued each in isolation and never composed them. Composed, rev 1's versions would have moved feature work into the envelope (Decision 3) and supplied the approval that lets the envelope merge unattended (Decision 4) — **exactly the widening rev 1's own text denied**, against a parent non-goal that says logic, protocol, security and public-API changes *always* reach a human.

With Decision 3 withdrawn to its narrower claim, that composition no longer arises: the envelope membership is unchanged, and Decision 4 grants authority only over the four classes already inside it.

**This is recorded as a standing obligation: any future proposal that widens the envelope must be composed against Decision 4 before it is argued.**

---

## Decision 4 — Merge authority

This **reverses rung 6** (`merge.rs:59-63`, *"cannot become one"*). It is argued on merits, not on a claimed prior approval.

Three ordered steps. **The first is worth doing whether or not any of the rest is approved.**

**1. Required status checks on `main`.** Read 2026-09-12:

```
approvals: 1   enforce_admins: true   dismiss_stale: true
codeowners: false   last_push_approval: false   required_status_checks: absent
```

**No status checks are required.** Nothing at the branch level requires CI to pass; the sole gate is the human approval. That is masked today because a human reads every PR, and it means the 13 CI checks are advisory as far as protection is concerned. The objective gate must exist before any approval automation does.

**Stated plainly, because it is a reduction:** today `main`'s protection is *"a human read it."* After steps 1–3 it is *"software decided, and CI was green."* Goal 5 says deterministic gates alone are never sufficient authority — so step 1 is a prerequisite, **not** the thing that makes steps 2–3 safe.

**2. Fix #346.** A merge hold parks the issue in `agent:blocked`; `advance` skips that label; and `agent:blocked` resumes on *a newer human comment*. An approval is not a comment. **Today even a human approving does not resume a held PR.** Until this is fixed no PASS reaches a merge by any route.

**3. A second approving identity.** A GitHub App with `pull_requests:write` that posts an approving review when — and only when — `decide_merge` returns **`MergeDecision::EligibleForMerge`**.

> Rev 1 wrote this trigger as `WouldMerge`. That was **wrong and circular**: `WouldMerge` is a `MergeOutcome` variant (`merge.rs:330`) produced only under `dry_run` (`merge.rs:631`), *after* the approval guard at `merge.rs:610`. A bot firing on it would wait for a signal that requires the approval the bot exists to give.

Two constraints already agree with this: `dismiss_stale: true` requires the approval to follow the last push, and rung 6 refuses to merge unless the reviewed SHA is still the head (`merge.rs:544-562`).

**Additional rule: a reused verdict must not trigger an approval.** `advance` caches a review by `(pr_number, head_sha, base_branch)` and prints `already reviewed`; two of the live run's passes did exactly that. Goal 5's wording is *"having read the diff"* — a reused verdict is a judgment nobody read the current state to reach. **The approving identity fires only on a review performed against the current head in this pass.**

**Honest limits, which rev 1 presented as security properties:**

- **Distinct is not independent.** The "only when" is enforced by local code, on the same host, in the same trust domain as the agent whose work is being approved. GitHub sees a credential with `pull_requests:write`; it cannot see `decide_merge`. Anything that reaches the key can approve any PR in scope, including one it opened. This satisfies GitHub's mechanical anti-self-approval check while supplying none of the independence that check exists to create.
- **It raises the payoff of prompt injection.** The loop reads attacker-influenceable text — issue bodies, PR comments. Today a successful injection gets a PR a human reads. After this, it gets a merge to `main`.

**A threat model is a prerequisite, not a follow-up.** The repo already keeps `2026-08-17-humanlayer-threat-model-design.md`, and the parent spec devotes real space to the deny-list and credential prohibitions. Required coverage: where the App private key lives, rotation, repo- vs org-scoping, behavior if the Autopilot host is compromised, and whether the parent spec's deny-list on default-branch pushes reaches an API merge (by rev 11's own reasoning exempting `gh pr create`, it does not).

**The tradeoff, so it is chosen rather than discovered.** This moves the human decision from *"approve this diff"* to *"label this issue"* — which happens **before the diff exists**. With Decision 3 withdrawn, nothing restores diff-sighted judgment later. That is the cost, and it is why the envelope stays at four low-risk classes.

---

## Decision 5 — Convergence is an early exit, not the bound

**Rev 1's version is withdrawn.** It proposed continuing while findings were novel and stopping when they repeated. Its own text supplies the refutation: if a reviewer "can find new things on unchanged code," novelty never runs out, and the loop has no termination proof. It replaced a counter that provably terminates with a judgment-derived signal that does not — and #339, cited as supporting evidence, is a **counterexample**: all three of its rounds were novel, so the detector would have scored each as progress and run *longer* without merging.

**The bound stays a counter.** Per the design rule above, only a counter can supply termination where no objective gate constrains the outcome.

**What changes is that the counter is the remediation's own** (#344). #339 got three rounds not by policy but because writing the document consumed 2 of its 5 attempts. A remediation must have a budget that does not depend on how hard the original work was — today, *the harder an issue was to get right, the less budget it has to answer review.* An issue that succeeds on attempt 5 of 5 has none.

**Convergence detection is added as an early exit only.** It may end a loop *sooner*; it may never extend one. This preserves what `remediate.rs:103-104` was protecting — that the human is **told** — because the counter still always fires.

Failure modes the detector must handle, all identified by rev 1's review:

- **Oscillation.** A→B→A→B is novel against the previous round and repeats with period 2. Comparison is against *all* prior rounds, not the last.
- **Rewording.** A reworded finding must not read as new.
- **Regression.** A remediation that introduces a defect produces a genuinely new finding; findings can strictly increase without anything being wrong with the detector.
- **Trivial novelty.** One new typo per round continues the loop at full review cost.
- **Degenerate verdicts.** Zero findings with `NeedsChanges`, or findings alongside a PASS.
- **The regress.** "Findings repeat or overlap" is itself a semantic judgment over free-text prose from a nondeterministic model — a model bounding a model. The thrash-detection analogy does **not** transfer: attempt signatures are structured and deterministic; review findings are not. **Because the detector is only an early exit, its being fooled costs rounds, never termination.** That is the whole reason for the demotion.

**On spend as a bound.** Rev 1 proposed "count reviews separately" as new. It already exists: `DEFAULT_MAX_UNPRICED_REVIEWS_PER_DAY = 20` (`review.rs:129`), documented as *"the only bound on reviewer spend that actually holds today."* What is genuinely absent is a per-*invocation* dollar ceiling, because `codex exec` emits no price and has no `--max-budget-usd` equivalent (`review.rs:45-48`). Any wall-clock bound must name a value and an exhaustion behavior; rev 1 named neither.

**Review depth should be routed, not uniform** — a deep multi-lens review may be cheaper end-to-end than N cheap rounds plus N remediation dispatches. Two cautions: depth is not accuracy (`ultrareview` ran ~75% false-positive on #273; the single Codex reviewer went 9-for-9 on #343 — **two tools, two diffs, two classes, n=1 each, not yet one measurement**), and an auto-fixing reviewer is a second writer on the branch, which rung 11 scoped out and whose "all fixed" reports have been true-but-uncommitted eight times here.

---

## Testing

Rev 1 had no Testing section, in a document whose thesis is that unvalidated termination is the problem. Required before any rung ships:

- **The counter terminates.** A property test: for any sequence of reviewer verdicts, the remediation loop halts within its own cap, and the human-notification path fires on exhaustion.
- **The early exit never extends.** A test asserting the detector can only reduce round count — mutation-checked, so deleting the constraint fails.
- **Each detector failure mode above** gets a case: oscillation with period 2, a reworded finding, a regression finding, trivial novelty, and both degenerate verdicts.
- **The approval identity never fires on a cached verdict** — a test that `already reviewed` produces no approval.
- **A remediation's budget is independent of the original work's** — boundary case: success at attempt N of N, followed by a remediation that still dispatches.

---

## Consequences

- The IC's contract changes from *issue → PR* to *sub-issue → PR*. Because units are real issues, existing bookkeeping is unchanged — but decomposition itself is a new step with a new failure mode (a wrong split) and no recovery.
- Sequential merging makes wall-clock scale with unit count.
- **Cost under decomposition is unestimated.** One live run cost $7.05 of a $25 ceiling for a single 224-line document that did not merge. Decomposition multiplies PRs, each carrying at least one review whose per-invocation spend cannot be metered. Whether a decomposed issue finishes inside one day's ceiling is an open question, not an answered one.
- A second approving identity is a real credential with merge reach, bounded only by `decide_merge` failing closed.
- **There is no post-merge failure path.** Every mechanism here is preventive: no revert path, no detection of a merge that should not have happened, no containment. The parent spec maintains an error table; this amendment adds an irreversible action without extending it. **This is a gap, not an omission, and it blocks step 3.**

---

## Migration

1. **Required status checks on `main`.** Valuable alone; prerequisite for everything in Decision 4.
2. **#346** — the `agent:blocked` trap. Required before any PASS can merge, by any route.
3. **#344 + #345** — remediation budget and exhaust close-out. Required before any round policy means anything.
4. **Convergence early-exit** (Decision 5), on top of the retained counter.
5. **Decomposition into sub-issues** (Decision 1). **Blocked on open question 1** — where decomposition lives materially changes what is built.
6. **Work-shape axis** (Decision 2). **Blocked on open question 2** — the refactor case is unsolved.
7. **Approving identity** (Decision 4.3) — last, and only after a threat model and a post-merge failure path exist.

---

## Open questions

1. **Where does decomposition live?** This restates the parent spec's **open question 16** (line 715) — *"Whether the Lead reuses `evaluate-issue` … or classifies independently"* — rather than raising a new one. Blocks migration step 5.
2. **How are refactors bounded?** They neither decompose cleanly nor hide behind flags. Blocks migration step 6.
3. **What mechanical check would make a flag's inertness verifiable?** Without one, Decision 3 makes no claim on the envelope.
4. **Is deep review actually cheaper than N cheap rounds?** Measurable now against ground truth: PR #343 carries nine hand-verified findings.
5. **Does a reviewer PASS mean anything on a class with no oracle?** If `documentation` cannot reliably converge it may belong *outside* the auto-merge envelope despite being low-risk — inverting the envelope's current membership.
6. **What is the post-merge failure path?** Blocks migration step 7.
7. **When is stacking worth the ripple?** Sequential is the safe default; there may be a size at which it is too slow.

---

## Evidence log

- **First end-to-end live run**, 2026-09-12/13, #339 → PR #343, **$7.05**, **9 dispatches** across four Lead ticks (2 + 3 + 1 + 3, per tick summaries), **3 fresh reviews** at 3 distinct head SHAs, plus 2 passes that reused a cached verdict.
- **Review rounds:** 3 → 4 → 2 findings; nine distinct, none repeated. **Verified by the same agent that wrote rev 1 of this document** — self-verification, not independent confirmation.
- **Defects found by living through them:** #344, #345, #346 — all open.
- **The ladder:** eleven rungs as PRs #318-#320 and #322-#329, each merged independently; #317 is the design spec. Rungs 9 and 11 off by default; rung 10 enabled with `--merge` withheld. **Every one human-planned and human-approved.**
- **Prior decomposition:** #283 → #297 → #298 → #299, all merged — with #299 revising the split mid-flight.
- **Branch protection read** 2026-09-12: no required status checks.
- **Reviewer cost:** `review.rs:45-48` — `codex exec` emits no price and has no per-invocation dollar ceiling; `review.rs:129` — a per-day invocation cap does exist.
