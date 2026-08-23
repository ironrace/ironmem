# Autonomous Backlog Runner — Design

**Date:** 2026-08-21
**Scope:** A new, self-contained ironmem subsystem that works labeled GitHub backlog issues to a mergeable state across all write-access repos, without a human dispatching each one. Does not touch `collab`, `iron-build`, `iron-spec`, or the HumanLayer epic.
**Status:** Draft
**Revision:** rev 2 — incorporates two review passes. Changes from rev 1 are marked ⟨r2⟩ where they alter a decision or add a mechanism.

> Working name: **Autopilot**. Provisional — see Open Questions.

---

## Problem

The backlog only moves when a human is personally driving a session.

Concretely, in this repo alone there are 27 open issues; the oldest (#156/#157/#158, additional harness support) were filed 2026-06-21 and have gone two months untouched. Every existing execution path requires the human attached for its full duration:

- **`collab`** is human-gated *by design*. The approval gates at `canonical`, `final`, and `final_review` are load-bearing protocol invariants, not incidental friction. Removing them is not a tuning exercise; it is a different system.
- **`iron-build`** runs a plan to completion, but only inside a session a human started and stays attached to. Close the session, the work stops.
- **`wiggum`** (`/Users/jeffreycrum/git-repos/jcagentszero/wiggum_glp1.py`) does run unattended, but every phase is a fresh one-shot `claude -p` subprocess that starts blind. Its only memory of a prior failure is `fail_detail[-2000:]` pasted into the next prompt, and a task that fails twice (`FIX_ATTEMPTS = 2`) is reverted with `git checkout .` and discarded entirely.

So throughput is bounded by human attention and prompt count, not by what agents can actually do. For calibration, a comparable setup inside Anthropic runs 30–50 human prompts/day against 40–100 concurrently working agents across 8–10 projects — roughly two orders of magnitude from where one-session-at-a-time lands.

Second, distinct problem: **nothing improves with repetition.** Every attempt starts from zero. There is no durable record that "approach X was tried on this issue and failed for reason Y," so the same dead ends remain re-explorable forever, at full cost, indefinitely.

This is not an ironmem-only problem. It applies across all repos — ironmem is where the code will live, not the only target.

---

## Goals

1. An issue labeled eligible is carried to a mergeable state — branch, commits, green gates, independent review, and either an auto-merge or an open PR — without a human dispatching or supervising it.
2. Works across heterogeneous repos (Rust, Python, Swift/iOS, and others) with **no per-repo code changes** to the runner.
3. Failed approaches are recorded durably and consulted by later attempts, so an IC does not re-explore a known dead end.
4. The human approves **envelopes** — which repos are eligible, what "green" means there, which issues are in scope, which changes may merge unreviewed — never individual steps.
5. ⟨r2⟩ **No change reaches the default branch without either a human or an independent fresh-context reviewer having read the diff.** Deterministic gates alone are never sufficient authority to merge.
6. Resource consumption is bounded and observable before the bill arrives, not after.

---

## Non-goals

- **The AVO commit-on-improvement loop is not in v1.** It requires a continuous scoring function and serves roughly 10% of the current backlog (the abeval-scorable skill-improvement issues, ~#301/#302/#303) while carrying most of the new complexity. It is designed for as an additive v2 — see *Extension point* — not built now.
- **No Project Lead tier.** Deferred until a single repo carries enough concurrent work that one Lead cannot hold its context.
- **No second peer Lead.** Mutual heartbeat/restart between two Leads solves a failure mode a cron watchdog also solves at this scale.
- **No changes to `collab`, `iron-build`, `iron-spec`, or `iron-tdd`.** This is a sibling subsystem. It does not extend the collab protocol, use `collab_send`/`collab_recv`, or alter any existing gate.
- **Not part of the HumanLayer epic (#304–#312)** and does not depend on its outcome.
- **Does not auto-merge logic, protocol, security, or public-API changes.** Those always reach a human.
- **Does not touch unlabeled issues.** An issue is invisible until explicitly opted in.
- **No container/VM sandbox.** Universal containerization is impossible here — `xcodebuild` requires the macOS host — and maintaining two execution paths where the weaker one serves the iOS repo is worse than one honest path.
- ⟨r2⟩ **No fork-based workflow. v1 requires push access** to the target repo (feature-branch push + PR). Read-only reference clones (`python-repos`, `llama_index`) are not targets.
- ⟨r2⟩ **No IC self-review.** The reviewer is always a separate, fresh-context agent. An IC grading its own diff carries the same blind spots that produced it.

---

## Architecture

### Two tiers, four roles

```
        cron watchdog  ──(restarts if wedged)──►  Lead
                                                    │
                          SendMessage / ListAgents  │  (harness-native mesh)
                                                    │
                    ┌───────────────┬───────────────┴───────────────┐
                    ▼               ▼                               ▼
                  IC #275         IC #283                        IC #296
              (own worktree)  (own worktree)                 (own worktree)
                    │
                    └─ on green ─► Reviewer (fresh context, short-lived, read-only)

        Onboarder (one-shot, human-invoked, per repo — not part of the run loop)
```

**Lead** — one long-running `claude` session. Owns cross-repo prioritization, dispatch, both supervision checks, the budget ledger, merge execution, and is the human's interface. A plain external cron watchdog restarts it if it wedges; the Lead is not responsible for its own resurrection.

**IC** — one per in-flight issue. **Separate OS processes, not in-session subagents.** This is forced, not chosen:
- the Lead must run health checks *while* ICs work, which an in-session subagent's blocking turn prevents;
- in-session subagents die with their parent session, making multi-day autonomy impossible;
- a wedged IC must be killable and restartable independently of its siblings.

**Reviewer** ⟨r2⟩ — a short-lived, fresh-context, read-only agent the Lead dispatches once an IC's PR is open. It performs **both** merge-time jobs: re-classify the diff's risk, and review the diff for correctness/security, returning `PASS` or `NEEDS CHANGES`. Routed to **Codex** via `launcher`'s existing multi-harness support, giving cross-model adversarial review rather than same-model self-agreement. Not a tier — it supervises nothing and holds no state.

**Onboarder** ⟨r2⟩ — a one-shot agent, invoked by a human per repo, that infers gate commands. Outside the autonomous loop entirely.

### Reuse: `crates/ironmem/src/launcher/`

The spawn path is largely already built. `launcher` validates the assistant binary, canonicalizes and warms the target repo, ensures the ironmem MCP server is registered, and launches the assistant with the repo as its working directory — all registry-driven through `crate::harness::REGISTRY` across `claude`/`codex`/`grok`/`gemini`. This is what makes a Codex reviewer nearly free.

### Transport

Lead↔IC messaging uses the **harness-native `SendMessage`/`ListAgents` mesh**, which addresses other local Claude sessions by name. Explicitly *not* `collab_send`/`collab_recv`: that mailbox is a bespoke two-party (Claude↔Codex) protocol built for bounded human-gated turns, and bending it into an N-way supervision mesh would create a second messaging layer with none of the native one's addressing.

> ⟨r2⟩ **Load-bearing assumption, not yet validated:** that an *unattended, long-running* IC process is simultaneously addressable via `SendMessage`/`ListAgents`. A one-shot `claude -p` is not an addressable session. If this proves false, supervision degrades to polling the dispatch-state drawers (below) instead of pinging, which the design supports but which changes latency and cost. **Validate before implementation** — see Open Questions.

### Repo onboarding (gate discovery)

`wiggum` knew what "green" meant because `VALIDATION_COMMANDS` was a literal dict with `backend → pytest/flake8` and `frontend → xcodebuild` typed into the source. That does not survive "all repos," and it is the safety-critical part: a wrong gate means confidently committing broken code.

⟨r2⟩ Placement, previously unstated:

1. Human runs `ironmem autopilot onboard <repo>`.
2. A one-shot Onboarder agent inspects the repo (CI config, `Cargo.toml`/`package.json`/`Makefile`) and writes a **proposed** gate config drawer in `pending` state.
3. Human reviews and runs `ironmem autopilot approve <repo>`; the drawer flips to `approved`.
4. **The Lead refuses to dispatch into any repo without an `approved` config.**

Config lives centrally in ironmem rather than as a file in each target repo, so repos whose tree you'd rather not add config to are still supportable — subject to the write-access requirement above.

### Who classifies, and when ⟨r2⟩

Risk classification happens twice, by two different actors:

| When | Actor | Purpose |
|---|---|---|
| At dispatch | **Lead** | Route the IC, set the expected class |
| Before merge | **Reviewer** (never the IC) | Classify the *actual diff*, plus review it |

Any mismatch between the two, or any reviewer uncertainty, **fails closed to a human PR**. Low-risk classes eligible for auto-merge on green *and* reviewer PASS: documentation, dependency bumps, mechanical renames, test-only changes. Everything touching logic, protocol, security, or public API opens a PR and waits for a human regardless of reviewer verdict.

### Who merges ⟨r2⟩

Rev 1 contained a contradiction: the deny-list blocked "any push to main/master" while the data flow ended low-risk work at `─► merge`. Resolved:

- **Every IC terminal state is an open PR.** ICs push feature branches only; pushing to a default branch is deny-listed for all ICs, without exception.
- **Auto-merge is the Lead merging that PR** via `gh pr merge` after reviewer PASS + matching low-risk classification. A GitHub API merge, not a local push, so the deny-list stays absolute.

### Two supervision checks, both required

An IC can be perfectly healthy and still completely stuck, so these are not redundant:

| Check | Question | Action |
|---|---|---|
| **process-health** | Is the IC alive and making progress? | Restart from last checkpoint |
| **strategy-health** | Is it alive but thrashing the same failure? | Redirect strategy, or stop and escalate |

⟨r2⟩ process-health must **not** be ping-alone: an IC mid-long-turn legitimately cannot answer. Declare it dead only when *both* a liveness ping goes unanswered past a short timeout **and** its checkpoint/lineage state has not advanced within a longer window.

⟨r2⟩ strategy-health as described watches one IC inside one session. That is insufficient on its own — see *Cross-dispatch stagnation* below.

### Cross-dispatch stagnation control ⟨r2⟩

Without this the system livelocks on budget: an IC exhausts its retries today → the issue keeps `agent:ready` → the daily budget resets at midnight → the Lead picks the same issue tomorrow → lineage prevents repeating the *same* approach but not another doomed one → repeat indefinitely. AVO's supervisor watched the whole trajectory; rev 1 ported only the within-session half.

- A **per-issue attempt counter persists across dispatches** in the issue's status drawer.
- On reaching the cap: append a terminal lineage record, post a comment summarizing everything tried, and flip the label to `agent:exhausted`.
- **`agent:exhausted` never self-resumes.** Only a human re-labeling it retries.

### Lead crash-safe state ⟨r2⟩

Rev 1 said a restarted Lead "re-adopts ICs via `ListAgents`" — but that reveals only who is *alive*, not what the Lead *knew*: which issue each IC owns, dispatch-time classes, the queue, spend so far. That lived only in the Lead's context window, which the watchdog's restart erases.

At dispatch the Lead writes a **dispatch-state drawer** (`logical_key` per in-flight issue): `{issue, repo, worktree_path, ic_session_name, dispatch_class, attempt_n, state, started_at}`, updated at each transition. On restart it reconciles that set against `ListAgents`:

| Drawer | Alive? | Action |
|---|---|---|
| present | yes | Adopt and resume supervision |
| present | no | Restart from checkpoint, or quarantine the worktree |
| absent | yes | Orphan — flag for human, do not silently adopt |

### Alternatives considered and rejected

| Alternative | Why rejected |
|---|---|
| Extend `collab` | Human-gated by design; its approval gates are invariants. Its mailbox is two-party. Unbounded autonomy is a different system, not a flag. |
| Fork `wiggum` wholesale | Its git/logging/backoff plumbing is worth reusing, but its core — one-shot disconnected `claude -p` per phase — is precisely what destroys lineage. Rewrite, don't patch. |
| Deterministic scheduler as top tier | Cannot itself wedge, but there is nothing to talk to, no cross-repo judgment, and supervision degrades to crude timeouts. The Lead is an interface, not just a dispatcher. |
| Two peer Leads (Daisy's shape) | Correct at 8–10 concurrent projects; premature here. A cron one-liner buys the same resilience at v1 scale. |
| Three tiers from the start | At v1 volume the middle tier mostly relays, while adding a process that can wedge, a message hop, and a place for context to go lossy. |
| Strict Bash allowlist for ICs | Gates are arbitrary per-repo commands; the allowlist cannot be enumerated in advance and would block legitimate work constantly. |
| Container per IC | `xcodebuild` needs the macOS host, so it can't be universal; two execution paths where the weaker serves the iOS repo. |
| ⟨r2⟩ Gates-only merge authority (rev 1) | Tests-green ≠ review-clean. A dependency bump to a vulnerable version, a semantics-changing "mechanical" rename, or a docs change asserting something false all pass gates. |
| ⟨r2⟩ Claude fresh-context reviewer | Viable, and one fewer harness dependency — but same-model reviewer and implementer share blind spots. Codex gives genuine cross-model adversarial review at near-zero marginal cost given `launcher`. Reversible: it is one registry id. |

---

## Data flow

```
issue labeled `agent:ready`
   └─► Lead: pick by priority:* order; check budget, concurrency cap, per-issue attempt cap
        └─► Lead classifies risk (dispatch-time) → writes dispatch-state drawer
             └─► create git worktree for this issue
                  └─► launcher spawns IC process (repo as cwd, MCP registered)
                       ├─► IC reads knowledge base K: search / code_map / kg_query
                       ├─► IC reads lineage for THIS issue: prior attempts + verdicts
                       ├─► IC implements
                       ├─► IC runs approved gates for this repo
                       │     └─ fail ─► append attempt to lineage ─► bounded retry
                       │                 └─ retries exhausted ─► `agent:exhausted`
                       └─► green
                            ├─► append SUCCESS attempt to lineage (with commit_sha)
                            ├─► push feature branch, open PR   [never pushes a default branch]
                            └─► IC checkpoints and exits
                                 └─► Lead dispatches REVIEWER (fresh context, Codex, read-only)
                                      ├─ (a) re-classify risk against the actual diff
                                      └─ (b) review diff → PASS | NEEDS CHANGES
                                           ├─ NEEDS CHANGES ─► re-dispatch IC to fix
                                           │                    (counts against the same
                                           │                     per-issue attempt cap)
                                           ├─ PASS + low-risk + class matches dispatch
                                           │      ─► Lead merges via `gh pr merge`
                                           └─ otherwise ─► PR stays open, labeled, human notified
                                                └─► Lead records outcome, cleans worktree
```

⟨r2⟩ Note that **every attempt appends a lineage record — successes included**, not only failures. The record shape always carried `commit_sha`; rev 1's diagram only wrote on failure, which would have left successful approaches unrecorded and therefore re-derivable.

### Storage — validated against the live MCP surface

**No new MCP tools are required for v1.** Confirmed by reading the tool schemas, not assumed.

| What | Where | Shape |
|---|---|---|
| Knowledge base (K) | existing `search`, `code_map_load`, `kg_query` | unchanged |
| Attempt lineage | new drawer room `backlog-lineage` | **append-only**: one drawer per attempt, `{issue, attempt_n, approach, verdict, why_failed, commit_sha}` |
| Per-issue current state | same room | **`logical_key`** per issue: best-so-far, cumulative `attempt_n` across dispatches |
| ⟨r2⟩ Dispatch state | same room | **`logical_key`** per in-flight issue; the Lead's crash-safe memory |
| ⟨r2⟩ Daily budget ledger | same room | **`logical_key`** per date; the Lead's authoritative spend record |
| Exact issue→attempt traversal | `kg_add` / `kg_query` | triples `issue-283 --has_attempt--> <attempt_id>` |
| Approved gate config | drawer, `logical_key` per repo | inferred + human-approved, `pending` → `approved` |

> **Design hazard — do not use `logical_key` for attempt records.** `add_drawer`'s `logical_key` *rewrites* the drawer in that wing/room. Applying it to attempts would silently overwrite each attempt with the next and destroy exactly the history this feature exists to keep. Attempts are plain `add_drawer` calls with no `logical_key`; only *status* drawers use one. This needs a regression test (see Testing).

Semantic `search` alone cannot reliably enumerate "every attempt on issue #283" — hence the `kg` edges for exact traversal.

### Budget accounting — validated, with a real gap ⟨r2⟩

Verified against `crates/ironmem/src/hook.rs`:

- **Good news:** `persist_transcript_tokens` is *not* collab-specific. It runs on `stop`/`precompact`, is registry-driven via `HarnessSpec::transcript_parser`, and is gated only by `metrics_enabled()` — so headless IC sessions are in scope in principle.
- **It is best-effort by design** (`hook.rs:451` — "warns on failure, never fails the hook"), so a parse or upsert failure silently under-counts spend.
- **It refuses incomplete data:** the Claude parser rejects any transcript lacking a terminal `result` event (`metrics/transcript.rs:108`). **A watchdog-killed or crashed IC therefore contributes zero recorded tokens** — the sessions most likely to have burned budget are precisely the ones invisible to `token_usage`.

Consequence: the Lead maintains its **own** daily ledger drawer as the authoritative budget record, incremented at dispatch/completion boundaries, and treats `token_usage` as a reconciliation source rather than ground truth. Relying on `token_usage` alone would make the stop-condition unenforceable in exactly the failure cases that matter.

### Secret handling on the lineage write path ⟨r2⟩

Lineage records embed gate output — test failures, stack traces, command stderr — which can contain tokens, credentials, and environment values. **No write-time redaction exists today.** Verified: `sanitize.rs::sanitize_content` only validates non-emptiness and length; `search/sanitizer.rs` handles *query* degradation; `config.rs::redacts_sensitive_content` is a **read-time output mode** governing what MCP returns, not what is stored. Content written to a drawer is persisted verbatim.

The lineage writer must therefore truncate and scrub gate output before persisting. This is **new work, not reuse.**

### Extension point (v2, AVO)

The lineage store is shaped so the AVO loop is additive: add a `score` field to the attempt record and a scoring function per issue class, then change the commit rule from *"correctness passes"* to *"correctness passes **and** score ≥ best-so-far."* No storage migration, no change to the tiering, no change to the transport.

---

## Error handling

| Condition | Behavior |
|---|---|
| IC process dies | process-health detects it; restart from last checkpoint. Lineage in drawers is unaffected. |
| ⟨r2⟩ IC alive but mid-long-turn (ping unanswered) | **Not** treated as dead. Death requires unanswered ping **and** no checkpoint advance in the longer window. |
| IC alive but repeats the same failure N times | strategy-health fires: redirect strategy, or stop and escalate. Never silent infinite retry. |
| ⟨r2⟩ Issue re-picked across days without converging | Per-issue attempt cap (persisted, cross-dispatch) → terminal lineage record, summary comment, `agent:exhausted`. Never auto-repicked. |
| Lead wedges or dies | Cron watchdog restarts it. ⟨r2⟩ It rebuilds state from dispatch-state drawers reconciled against `ListAgents`, not from context. |
| ⟨r2⟩ Lead restart finds a live IC with no dispatch drawer | Orphan: flag for human. Never silently adopted. |
| Gate command inferred wrongly | Caught at the human approval step. If wrong post-approval, the gate fails → task blocks and does **not** merge. Fails safe. |
| Diff's risk class ≠ dispatch-time class | Fail closed: PR stays open for human review. Never merge on the stale class. |
| Reviewer uncertain, or returns NEEDS CHANGES | ⟨r2⟩ Re-dispatch the IC to fix, counting against the same per-issue attempt cap. On exhaustion the PR stays open for a human — never merged with an unresolved finding. |
| ⟨r2⟩ Reviewer itself fails to run | Treated as NOT reviewed. No auto-merge; PR waits for a human. Infrastructure failure never becomes implicit approval. |
| IC hits a human-only decision | Message the Lead. Lead answers from cross-repo context if it can; otherwise posts the question on the issue, flips to `agent:blocked`. IC checkpoints findings to lineage and **exits** rather than holding a process. |
| ⟨r2⟩ Human answers a blocked issue | Lead polls `agent:blocked` issues for human comments newer than its own question, appends the answer to lineage, flips back to `agent:ready`, re-dispatches. Closes the one-way door rev 1 left open. |
| Concurrency cap reached | Lead queues the issue; does not dispatch. |
| Daily token budget exhausted | Lead stops dispatching and reports. In-flight ICs finish. |
| ⟨r2⟩ Killed IC's tokens never recorded | Expected, per the transcript-parser contract. The Lead's own ledger is authoritative; `token_usage` reconciles. |
| Rate limit hit | Exponential backoff (port `wiggum`'s `RATE_LIMIT_BACKOFF_BASE`/`_MAX`), then pause rather than hammer. |
| Two ICs on the same repo | Each has its own git worktree; no shared checkout, no interference. |
| Deny-listed operation attempted (force-push, **any push to a default branch**, write outside the worktree, touching credential/secret files) | Blocked, logged, IC flagged for human attention. |
| Worktree left dirty by a dead IC | Lead quarantines it and creates a fresh one rather than reusing dirty state. |
| ⟨r2⟩ Gate output contains secrets | Truncate + scrub at the lineage write path before persisting. No existing module does this. |
| Lineage drawers accumulate unboundedly | Retention policy + existing `ironmem memory gc`; attempt records for closed issues compact to a durable summary. |
| Issue is unlabeled | Invisible to the Lead. No work occurs. |
| ⟨r2⟩ Repo lacks push access | Rejected at onboarding. Not a supported target in v1. |

---

## Testing

| Test | Covers |
|---|---|
| Gate inference against fixture repos (Rust, Python, Swift) | Onboarder proposes correct commands for heterogeneous stacks |
| Work refused on a `pending` (unapproved) repo config | No repo runs on inferred gates alone (Goal 4) |
| Dispatch class `docs`, diff touches logic → PR, not merge | Double classification fails closed |
| Reviewer returns NEEDS CHANGES → no merge occurs | Goal 5 — gates alone never authorize a merge |
| ⟨r2⟩ Reviewer process fails to start → no merge occurs | Infrastructure failure ≠ approval |
| ⟨r2⟩ IC attempts a push to a default branch → blocked | Deny-list is absolute; merge authority is the Lead's alone |
| N failed attempts produce N distinct drawers | **Regression guard against `logical_key` misuse destroying lineage** |
| A successful attempt also produces a lineage record | Successes are recorded, not just failures |
| Per-issue status drawer overwrites rather than accumulating | `logical_key` used correctly where it *is* wanted |
| `kg_query` on an issue returns all its attempts | Exact traversal works where semantic search can't |
| Second attempt's prompt contains prior failure reasons | Goal 3 — dead ends actually consulted, not merely recorded |
| Kill an IC mid-task → Lead restarts it, lineage intact | process-health |
| ⟨r2⟩ IC silent but checkpointing → NOT declared dead | process-health false-positive guard |
| Feed identical failure repeatedly → thrash detection fires | strategy-health (within session) |
| ⟨r2⟩ Issue dispatched across N days hits the cap → `agent:exhausted`, never re-picked | Cross-dispatch stagnation; budget-livelock guard |
| ⟨r2⟩ Kill the Lead mid-flight → restart rebuilds state from drawers and re-adopts ICs | Crash-safe supervision |
| ⟨r2⟩ Live IC with no dispatch drawer → flagged orphan, not adopted | Reconciliation safety |
| ⟨r2⟩ Human replies to a blocked issue → auto-resumes and re-dispatches | Escalation is not a one-way door |
| ⟨r2⟩ Gate output containing a token → scrubbed before the drawer write | Secret leakage into persistent memory |
| Deny-list blocks force-push and writes outside the worktree | Blast radius |
| Two concurrent ICs on one repo don't interfere | Worktree isolation (cf. #310) |
| Concurrency cap respected under a flood of eligible issues | Stop conditions |
| Token budget exhaustion halts dispatch | Stop conditions |
| Unlabeled issue is never picked up | Intake envelope |

---

## Consequences

**Better**
- Eligible backlog work proceeds without per-issue dispatch; the human's role becomes envelope-setting and reviewing risky changes.
- Heterogeneous repos supported with no runner code changes per repo.
- Dead ends are recorded and consulted, so repeated attempts get cheaper instead of identical.
- ⟨r2⟩ Every merged change has been read by a fresh-context, cross-model reviewer — a stronger bar than most human-only workflows achieve in practice.
- Spend is bounded and observable up front.
- Substantial reuse: `launcher` (spawn + MCP registration + multi-harness), `harness` registry, `token_usage` metrics, drawers/kg, and `wiggum`'s git and backoff plumbing.

**Worse — the honest costs**
- A new long-running process to operate, monitor, and debug, with failure modes (wedged Lead, orphaned worktrees, stuck ICs) that don't exist today.
- **Unattended agents hold push access across all onboarded repos.** The deny-list plus worktree isolation are mitigations, not elimination: an IC can still do real damage inside its worktree and still reach the network.
- The risk classifier is a new correctness surface, and its *false negatives ship unreviewed code*. ⟨r2⟩ The Codex reviewer substantially mitigates this — a misclassification must now coincide with a reviewer PASS to ship unseen — but does not eliminate it.
- ⟨r2⟩ Every task now costs an extra reviewer run. Real marginal spend, accepted deliberately in exchange for Goal 5.
- ⟨r2⟩ Budget accounting is imperfect by construction: killed sessions contribute no `token_usage` rows, so the Lead's ledger is an estimate reconciled after the fact, not an exact meter.
- ⟨r2⟩ Secret scrubbing on the lineage path is net-new code on a path that persists raw command output. A bug there writes credentials into long-lived memory.
- Lineage drawers grow without bound absent an actively maintained retention policy.
- Per-repo gate approval is real friction before any repo becomes eligible.
- A second execution philosophy alongside `collab`/`iron-build` — two systems to keep coherent, and a standing temptation to converge them badly later.
- The `agent:ready` label is manual: forget to apply it and nothing gets worked. Autonomy is capped by a human remembering to opt issues in.

**Now committed to**
- The harness-native `SendMessage` mesh as agent transport.
- ironmem drawers + kg as the lineage, dispatch-state, and budget substrate.
- One git worktree per in-flight IC; the Lead as sole merge authority.
- ⟨r2⟩ Codex as the review harness (reversible — one registry id).

---

## Migration

No schema migration required. This is a new subsystem with no existing on-disk state to preserve; new drawer rooms are additive and `token_usage` (migration 008) already exists.

⟨r2⟩ **Three new GitHub labels**, following the existing `session:*` / `priority:*` namespace convention:

| Label | Meaning | Resume semantics |
|---|---|---|
| `agent:ready` | Opted in, eligible for dispatch | — |
| `agent:blocked` | Awaiting a human answer to a posted question | **Auto-resumes** when the Lead sees a newer human comment |
| `agent:exhausted` | Per-issue attempt cap hit | **Never self-resumes**; human must re-label |

Two blocked states rather than one, because their resume semantics genuinely differ: a question that has been answered should flow again on its own, whereas work the system already proved it cannot finish must not silently retry.

---

## Open questions

Tuning parameters, naming, and two validations. Only the validations gate implementation:

**Must validate before building**
1. ⟨r2⟩ **Is a headless, long-running IC addressable via `SendMessage`/`ListAgents`?** If not, supervision becomes drawer-polling. Affects the transport section directly.
2. ⟨r2⟩ **Does the `stop` hook fire for unattended IC sessions with `transcript_path` populated?** Ingestion is registry-driven and not collab-gated, but hook firing in headless mode is unconfirmed.

**Tuning, deferred to implementation**
3. **Name.** "Autopilot" is a working title.
4. Concurrency cap — initial `N` for in-flight ICs.
5. Daily token budget — the actual ceiling figure.
6. ⟨r2⟩ Within-dispatch retry bound, thrash-detection threshold, and cross-dispatch per-issue attempt cap — three distinct numbers.
7. Retention policy specifics — when attempt records compact to a summary.
8. ⟨r2⟩ Whether ICs (not just the reviewer) should route to Codex for some task classes.
9. Whether the Lead reuses `evaluate-issue` (DIRECT/IRON/COLLAB/SPLIT scoring, and its mandatory split above 15 tasks) for decomposition, or classifies independently.
