# Autonomous Backlog Runner — Design

**Date:** 2026-08-21
**Scope:** A new, self-contained ironmem subsystem that works labeled GitHub backlog issues to a mergeable state across all write-access repos, without a human dispatching each one. Does not touch `collab`, `iron-build`, `iron-spec`, or the HumanLayer epic.
**Status:** **Approved** — 2026-08-23, by Jeff Crum, at rev 4. Rev-5 amendment approved 2026-08-24. Rev 6 is validation-only (rung 0 of the `/goal` build ladder) and records measurement, not a design change requiring re-approval. Rev 7 records implementation findings from rungs 7-8 and closes two open questions; rev 8 records rung 9, which builds open question 9's residual; rev 9 records rung 10, which executes the data flow's post-green arc that rungs 5 and 6 built the pieces of. None changes an approved decision.
**Revision:** rev 9 — ⟨r9⟩ records rung 10, **the loop closing**: the arc this document's data flow draws from a green IC to a merged PR is now *executed*, by `autopilot advance` (`autopilot/advance.rs`). Rungs 5 and 6 built reviewing and merging; nothing joined them, so `queue::DeferReason::AlreadySucceeded` was a permanent dead end and a green PR waited on a human. No architectural change: `advance` decides only which issue is at which step, and `decide_merge` is still the single answer to "may this merge?". Earlier: rev 8 — ⟨r8⟩ records rung 9: open question 9's stated residual, the three judgment-shaped one-shot calls, is **built** (`autopilot/advise.rs`), off by default, and structured so the loop does not depend on it; the escalation a supervisor raises is now reported to a human on the issue rather than only in a drawer. No architectural change. Earlier: rev 7 — ⟨r7⟩ folds in the build ladder's own findings: open question 9 (is the Lead a Claude session?) is **closed** by rungs 7 and 8, and open question 7's wall-clock half is closed by rung 7. No architectural change; the two-tier, four-role shape is unchanged. Earlier: rev 2 incorporated two review passes (⟨r2⟩); rev 3 folded in the first validation round (⟨r3⟩); rev 4 folded in a controlled second round (⟨r4⟩) that reversed one rev-3 finding, retired one blocker, and settled the IC primitive; rev 5 (⟨r5⟩) **generalises that primitive by one parameter** and adds model routing; rev 6 (⟨r6⟩) is rung 0 of the build ladder — it measures the ⟨r5-doc⟩ claims for real, closes open question 6a with a working mitigation, and writes the turn-prompt template rev 4/5 never tested. See *Open questions*.

> ⟨r5⟩ **What changed from rev 4, in one line.** Rev 4 fixed the IC's supervision boundary at one process per *turn*. Rev 5 makes the number of turns per process a parameter, **N**, driven by a `/goal` condition inside the process. **Rev 4 is the N = 1 case and remains valid.** Nothing else about the architecture moves.

> ⟨r5⟩ **Provenance note.** The ⟨r4⟩ findings were *measured* on this machine. The ⟨r5⟩ `/goal` mechanics are *documented* behaviour (code.claude.com/docs/en/goal), not yet measured here. They are marked ⟨r5-doc⟩ where load-bearing, and rung 0 of the build sequence exists partly to measure them.

> ⟨r4⟩ **Read this first if you read rev 3.** Rev 3 recorded that push messaging to an unattended session fails. That was an artifact of the probe's own configuration, not a property of the system. Re-run as a controlled A/B, push messaging **works**. Rev 3's *Transport* section was wrong and is replaced. Separately, the transcript-ingestion breakage rev 3 called a prerequisite is no longer on the critical path.

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
     control: re-invoke `-p --resume` per dispatch  │  (pull)
               interrupt: SendMessage (abort only)  │  (push, best-effort)
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

⟨r4⟩ An IC is **not one long-lived process**. It is a *supervised re-invocation loop*: each turn is a fresh `claude -p --resume <uuid> --output-format json` process against a Lead-assigned session id, which exits when the turn ends. The session, not the process, is the durable thing. See *IC lifecycle*.

**Reviewer** ⟨r2⟩ — a short-lived, fresh-context, read-only agent the Lead dispatches once an IC's PR is open. It performs **both** merge-time jobs: re-classify the diff's risk, and review the diff for correctness/security, returning `PASS` or `NEEDS CHANGES`. Routed to **Codex** via `launcher`'s existing multi-harness support, giving cross-model adversarial review rather than same-model self-agreement. Not a tier — it supervises nothing and holds no state.

**Onboarder** ⟨r2⟩ — a one-shot agent, invoked by a human per repo, that infers gate commands. Outside the autonomous loop entirely.

### Reuse: `crates/ironmem/src/launcher/`

The spawn path is largely already built. `launcher` validates the assistant binary, canonicalizes and warms the target repo, ensures the ironmem MCP server is registered, and launches the assistant with the repo as its working directory — all registry-driven through `crate::harness::REGISTRY` across `claude`/`codex`/`grok`/`gemini`. This is what makes a Codex reviewer nearly free.

### Transport ⟨r4⟩

Lead↔IC coordination is **pull for control, push for interrupts**. Explicitly *not* `collab_send`/`collab_recv`: that mailbox is a bespoke two-party (Claude↔Codex) protocol built for bounded human-gated turns, and bending it into an N-way supervision mesh would create a second messaging layer with none of the native one's addressing.

**Control is pull.** The Lead directs an IC by writing the next dispatch's prompt, not by messaging a running one. ⟨r5⟩ Direction is therefore guaranteed to be read, arrives with no delivery dependency, and does not require the IC to be at a convenient point in its work. Status flows the other way through checkpoint and dispatch-state drawers the Lead polls.

**Push exists, and is reserved for interrupts.** Rev 3 recorded that push messaging to an unattended session fails. That was wrong — see the correction below. Push is used for exactly one job: aborting a turn already in flight (daily budget exhausted, human stop, issue closed upstream, duplicate PR merged). The alternative is `SIGKILL`, which discards that turn's spend and any uncheckpointed reasoning. Push is never used to assign work, because its latency is unbounded (below).

> ⟨r4⟩ **CORRECTS REV 3. Controlled A/B, 2026-08-23, Claude Code v2.1.241.**
>
> Same sender, same prompt, same machine, same minute; the receiver's launch flags were the only variable.
>
> | Receiver launched with | Delivered? | Observed |
> |---|---|---|
> | `--dangerously-skip-permissions` | **yes** | injected mid-turn, verbatim, after the first tool call (<10s); the IC acted on it and exited. 3 turns, $0.277 |
> | `--allowedTools Bash Write` | **no** | 24 tool calls over 299s; the message **never entered its transcript at all**. $0.798 |
>
> **Push delivery is gated on the receiver running in bypass-permissions mode.** ICs carry `--dangerously-skip-permissions` by design (see blast radius), so push is available to this system. Rev 3's probe lacked the flag; its negative result described the probe, not production.
>
> **The non-delivery path is an approval queue, not a silent drop.** A message to a non-bypass session is held for the *recipient user's* approval and expires unapproved. The **sender** receives two out-of-band notices — "held for approval", then "not approved before expiry" — minutes later. The receiver is never told anything. This is diagnostically useful and operationally useless: the notice arrives long after the moment delivery mattered.
>
> **Mechanics.** Transport is a Unix domain socket at `/tmp/cc-socks/<pid>.sock`, mode 0600. The receiver records `origin: {kind: "peer", from: "uds:…", verifiedPeerPid, msg_id, name, fromMode}` and wraps the body in `<cross-session-message>`. Discovery works through both `ListAgents` and `claude agents --json`.
>
> **Latency is bounded by the IC's tool-call cadence, not by the socket.** Injection lands only *between* tool calls. The control receiver's 24 separate sleeps offered 24 injection points; an IC sitting inside a single 10-minute `cargo test` offers one, at the end. Push is best-effort-fast and can never be the basis of a timeout or a liveness check.
>
> **Security boundary is same-user.** Any local process running as the same user can address an IC over its socket; `verifiedPeerPid` identifies the sender but does not restrict it. Acceptable on a single-user machine, and worth stating rather than assuming the mesh is authenticated beyond that.

**A coupling to keep in view:** push works *only* in bypass mode. Any future decision to run an IC without `--dangerously-skip-permissions` therefore also, and silently, removes the ability to abort it cleanly. The blast-radius decision and the interrupt channel are one decision, not two.

### IC lifecycle — the session primitive ⟨r4⟩⟨r5⟩

Rev 3 left this as the highest-value open question: `claude -p` is one-shot, so what is a long-lived IC? The answer is that **an IC does not need a long-lived process, only a long-lived session.**

⟨r5⟩ Rev 4 answered that with *one process per turn*. That was right about the session and wrong about the boundary: it assumed the Lead must own **every** turn boundary, when what the Lead actually needs is a boundary often enough to supervise. Those are different numbers.

**One dispatch = one process = N turns.** The Lead assigns a UUID and invokes:

```
claude -p "/goal <the repo's approved gate condition> or stop after N turns" \
  --session-id <uuid>              # first dispatch only; --resume <uuid> thereafter
  --output-format json \
  --name ic-<repo>-<issue> \
  --model <per risk class> \
  --dangerously-skip-permissions \
  --max-budget-usd <per-dispatch ceiling> \
  --max-turns <hard bound>
```

Inside the process, `/goal` runs the turn loop: after each turn a small fast model judges the condition against the transcript and either starts another turn or stops. The process exits, the Lead reads the result JSON, banks the cost, decides, and invokes the next dispatch against the same session id.

**N is the one new knob.** It trades supervision granularity against Lead cost:

| N | Behaviour |
|---|---|
| `1` | Exactly rev 4. The Lead composes every turn. Maximum control, maximum Lead spend. |
| `5–8` | Suggested starting range. The Lead supervises several times per issue; intra-dispatch iteration is priced at the evaluator's model, not the Lead's. |
| unbounded | A pure goal loop. The Lead sees one result at the end and supervises nothing in between. Rejected — see *Alternatives*. |

The reason N > 1 pays is that a Lead turn is not free. At N = 1 an Opus Lead spends one of its own turns per IC turn, plausibly exceeding the cost of the work being supervised. At N = 6 that supervision cost drops roughly six-fold while the Lead still gets six checkpoints per issue.

**Measured 2026-08-23 ⟨r4⟩:**

| Claim | Result |
|---|---|
| `--session-id` + `--resume` restores context across *separate processes* | ✅ a secret planted in turn 1 was recalled verbatim by a fresh process in turn 2 |
| Resume is affordable | ✅ turn 1 created 53,763 cache tokens at $0.538; turn 2 read all 53,763 from cache at **$0.028 — ~5%**. Entries are `ephemeral_1h`, so invocations less than an hour apart ride the cache |
| `--output-format json` yields a usable meter | ✅ returns `total_cost_usd`, full `usage`, `num_turns`, `duration_ms`, `permission_denials`, `session_id`, `is_error` |
| `claude agents --json` enumerates sessions without a TTY | ✅ lists interactive and background sessions — the Lead can enumerate ICs from plain Rust |
| `-p` sessions report busy/idle | ❌ **no `status` field** (interactive sessions have one). The registry gives liveness only |
| `--bg` background agents are drivable | ❌ they persist for weeks but do **not** appear in `ListAgents` as addressable peers |

**Documented, not yet measured ⟨r5-doc⟩:**

| Claim | Source |
|---|---|
| `claude -p "/goal …"` runs the whole multi-turn loop to completion in one invocation | docs |
| The evaluator judges **only what is already in the transcript**; it runs no tools | docs |
| Verdicts are met / not yet met / **impossible**; the last clears the goal and ends the loop | docs |
| Several turns with no tool use halts the loop and returns control with the goal still set | docs |
| Auth failure, exhausted credits, unrecoverable context overflow, and model-unavailable clear the goal; rate limits and overloads do **not** | docs |
| The condition may be up to 4,000 characters and doubles as the opening directive | docs |

**What the parameterisation buys:**

- **Most of the turn runner disappears.** The inner loop is a flag rather than orchestration code the project owns and debugs.
- **Intra-dispatch iteration is cheap.** The evaluator runs on the small fast model; see *Model routing*.
- **No cache-TTL exposure inside a dispatch.** One continuous session has no gap between its turns. The 1-hour window now only applies *between* dispatches.
- **Anti-stall and background check-in behaviour come free** rather than being reimplemented.

**What it costs, honestly:**

- **Checkpoint cadence stops being free.** Rev 4 got it as a consequence — the process was about to die, so the IC checkpointed. At N > 1 intra-dispatch turns are evaluator-driven, so **the IC must be explicitly instructed to checkpoint every turn**, and that instruction is now load-bearing rather than structural.
- **Thrash detection slows by up to N turns.** strategy-health evaluates per dispatch; a doomed approach can burn N turns before the Lead sees it. This is the main argument for keeping N small.
- **Budget granularity coarsens from turn to dispatch.** A killed process forfeits one dispatch's accounting rather than one turn's. Still bounded, still exact for everything already banked.
- **Preemption granularity coarsens too.** Budget exhaustion, a closed issue, or a higher-priority arrival are acted on at dispatch boundaries — or, mid-dispatch, over the abort channel (see *Transport*).

**Two requirements this introduces, neither of which existed in rev 4:**

1. ⟨r5⟩ **One definition of "done."** The `/goal` condition and the repo's approved gate config must be the *same* expression, generated from the gate config rather than written separately. Two sources of truth produce an IC that returns "met" over red gates.
2. ⟨r5⟩ **"Impossible" must be distinguishable from "met."** The evaluator can judge a condition unsatisfiable, which clears the goal and ends the invocation — a normal-looking exit. If the Lead cannot tell that from success it will treat abandonment as completion. Whether the result JSON distinguishes them is **unverified**; see *Open questions*.

**Also unchanged from rev 4:** `--name` gives ICs deterministic addresses (`ic-ironmem-283`) for both `ListAgents` and the abort path, and `--json-schema` lets the Reviewer's verdict be schema-validated rather than parsed out of prose.

### The turn-prompt template ⟨r6⟩

⟨r6⟩ Rung 0 built and ran this template against a real gate command in this repo (`cargo fmt --all -- --check`, read-only), across two separate processes joined by `--resume`, and separately probed it with a deliberately unsatisfiable condition. It worked as designed, with one required addition — see *6a, closed* below.

**The condition text (fills the `"..."` in the CLI invocation above):**

```
You are an IC dispatch for issue <repo>#<issue_number>: "<issue title>".

<issue body, or a summary if it exceeds budget>

Prior attempts on this issue (read before doing anything else):
<lineage: one line per past attempt — approach, verdict, why_failed — or
"none yet" for attempt 1>
<if a strategy redirect is in force: state it explicitly here, e.g.
"Do not retry <approach>; it failed for <reason>. Try <redirect> instead.">

Constraints: feature-branch push only, never push to the default branch. Stay
inside your worktree. Never touch credential or secret files.

Checkpoint your progress (what you tried, current state, next step) after
EVERY turn, not just at the end — you may be re-invoked as a fresh process
with only this checkpoint and the transcript to resume from.

The gate condition for this repo (generated from its approved gate config,
never authored separately): <gate command(s) and their required exit
status/output, verbatim from the approved config>.

Report your verdict using the required output schema when, and only when,
you have either satisfied the gate condition above or determined it cannot be
satisfied. Do not guess; if you are unsure whether it is met, the verdict is
not_met and you take another turn.

or stop after N turns
```

**Required CLI addition, not optional:** `--json-schema
'{"type":"object","properties":{"verdict":{"type":"string","enum":["met","impossible","not_met"]},"reason":{"type":"string"}},"required":["verdict","reason"],"additionalProperties":false}'`.
This is what makes 6a mechanically answerable — see below. `reason` is what the
Lead's dispatch-state drawer and the eventual lineage record quote verbatim; it
does not need separate free-text summarization.

**Measured 2026-08-24 ⟨r6⟩:** two dispatches on one session id, joined by
`--resume`, driving the real `cargo fmt --all -- --check` gate in this repo:
dispatch 1 (5 turns, first invocation) cost $0.187 and produced a checkpoint
file; dispatch 2 (1 turn, `--resume`) recalled the gate command, its exit
code, and the checkpoint's summary **verbatim, without re-running the gate**,
for $0.036 — about 19% of dispatch 1's cost. This is the r4 resume-cost
finding reproduced against the template's own phrasing rather than a planted
secret, and against a real gate command rather than a synthetic one.

### 6a, closed ⟨r6⟩

**The base result JSON does not distinguish "met" from "impossible."**
Confirmed by a controlled A/B: a trivially satisfiable condition and a
deliberately unsatisfiable one (retrieve a CA root's private key) produced
**identical** `terminal_reason` (`"completed"`), `stop_reason` (`"end_turn"`),
`subtype` (`"success"`), and `is_error` (`false`). No field, at any depth of
the JSON, names a verdict. A Lead reading only those fields cannot tell
abandonment from completion, exactly as OQ6a warned.

**Mitigation, tested and working: force the verdict through
`--json-schema`.** Re-run with the schema above, both arms returned a
top-level `structured_output` object — `{"verdict":"met","reason":"..."}` for
the satisfiable arm, `{"verdict":"impossible","reason":"..."}` for the
unsatisfiable one — cleanly distinguishable by exact string match, no
transcript parsing or second LLM judgment required. `stop_reason` changes to
`"tool_use"` under the schema (structured output is implemented as a forced
final tool call), which does not interfere with the IC's normal tool use
earlier in the dispatch. **This is now a required element of the turn-prompt
template, not an optional nicety** — see above. Cost overhead was visible but
not characterized: the schema-constrained satisfiable-arm probe cost $0.237
against $0.128 for the unconstrained equivalent, plausibly fixed
schema/tool-registration overhead rather than a per-turn cost; this needs
re-measurement against a real (non-trivial) IC workload before being treated
as a rate rather than a one-time cost.

### Model routing ⟨r5⟩

Four distinct model slots, decided 2026-08-24:

| Slot | Model | Why |
|---|---|---|
| Goal evaluator | **small fast model (Haiku default)** | Its errors are bounded on both sides: a false *met* is caught by the Lead's gate check and then the reviewer, costing one dispatch; a false *not met* is capped by the `stop after N turns` clause. It is a loop-continuation heuristic, never an authority. |
| IC | **Sonnet**, escalating to Opus by risk class | Where capability converts into merged rather than abandoned issues. Ports `wiggum`'s existing model-tiering. |
| Lead | **Opus** | Cross-repo prioritisation and judgement — and at N > 1 it runs far fewer turns. |
| Reviewer | **Codex** | Cross-model on purpose. Unchanged from rev 2. |

**Do not upgrade the evaluator to buy accuracy.** Two reasons.

First, `ANTHROPIC_DEFAULT_HAIKU_MODEL` is read *everywhere* the small fast model is used — it also re-points the `haiku` alias and background work such as conversation summarisation — so the evaluator cannot be upgraded in isolation. If it is ever done, set it in the IC's spawn environment only, which `launcher` already controls, never in a settings file.

Second and more important: the evaluator reads the IC's **own** transcript, so it is a same-family judge of an agent's self-report — the very blind spot this design refuses elsewhere ("No IC self-review"). A stronger evaluator makes that self-judgement more articulate, not more independent. The structural answer is the fresh-context cross-model reviewer already downstream. **An urge to upgrade the evaluator should be read as a signal that the gate condition is written too softly.**

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

⟨r9⟩ **Who actually runs that sequence — rung 10.** Rungs 5 and 6 built the review and the merge as commands an operator invoked per PR; rung 8 deliberately kept both out of the Lead tick, because chaining them would make the smallest unit of Lead activity the largest blast radius. That left the arc unexecuted: an IC went green, `run_issue` recorded the success and cleared the dispatch state, and the issue sat labeled `agent:ready` with an open PR nothing would look at — reported forever as `AlreadySucceeded` and acted on by nobody.

`autopilot/advance.rs` closes it, as a **second command rather than inside the tick**, so rung 8's decision stands unchanged. `advance_pass` finds each succeeded issue's open PR by head branch, reviews it when no review has read the PR's current head commit, applies rung 6's merge authority, and — once the PR has landed — clears the dispatch-state drawer and removes the worktree, which is the spec's *"Lead records outcome, cleans worktree"* step and had never been built. An operator's cron runs `lead` then `advance`.

Five decisions in it are load-bearing:

1. **The gate is green only when the PR head *is* the commit the gate was green at.** A branch with commits pushed after the green run has unverified code at its head, and `decide_merge` authorizes an auto-merge on green. An unknown green commit answers false — unknown fails closed.
2. **The review trigger is "no review has read *this* commit", not "no review exists"** — the same equality rung 6 enforces before merging. An IC that pushed a fix is re-reviewed automatically; a PR whose head has not moved is never re-billed.
3. **Merging is opt-in (`--merge`); without it every merge is *rehearsed*.** Every guard and every read still runs and nothing is written to GitHub. Reviewing is not gated, because it is already-authorized activity under rung 5's own ceilings; merge is the only irreversible action in the subsystem. Rung 9's precedent, applied to an irreversible action rather than a paid one.
4. **The class is the `risk:*` label or `unclassified`, and no advisor is asked.** This value is half of `decide_merge`'s `ClassMismatch` test, so it is exactly the input deciding whether a PR merges *without a human*. Requiring a human to have written `risk:documentation` before that can happen is the authorization, not a gap in the automation.
5. **A missing worktree stalls a review rather than falling back to the main checkout.** A reviewer reads the diff from the checkout it is pointed at; one that does not contain the branch does not fail, it writes a confident review of something else — and that review authorizes a merge. It is also why the worktree is removed *after* the PR lands and never when the IC goes green: the worktree is the reviewer's input.

⟨r9⟩ **Residual, stated not hidden.** The data flow's *"NEEDS CHANGES → re-dispatch IC to fix"* arrow is still not executed. A held PR is reviewed, commented and labeled exactly as rungs 5 and 6 specify, and then waits for a human; re-opening work whose lineage records a success is a distinct capability with its own hazards, and it is not built. Rung 10 closes the green path only.

### Two supervision checks, both required

An IC can be perfectly healthy and still completely stuck, so these are not redundant:

| Check | Question | Action |
|---|---|---|
| **process-health** | Is the IC alive and making progress? | Restart from last checkpoint |
| **strategy-health** | Is it alive but thrashing the same failure? | Redirect strategy, or stop and escalate |

⟨r2⟩ process-health must **not** be ping-alone: an IC mid-long-turn legitimately cannot answer. Declare it dead only when *both* a liveness ping goes unanswered past a short timeout **and** its checkpoint/lineage state has not advanced within a longer window.

⟨r2⟩ strategy-health as described watches one IC inside one session. That is insufficient on its own — see *Cross-dispatch stagnation* below.

⟨r5⟩ Both checks now run **per dispatch**, not per turn. process-health is unaffected in kind — the registry gave liveness only in any case. strategy-health is affected in degree: a doomed approach can burn up to N turns before the Lead sees it. N is therefore a supervision parameter as much as a cost one, and should start small.

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
| Fork `wiggum` wholesale | Its git/logging/backoff plumbing is worth reusing, but its core — a **disconnected** `claude -p` per phase, starting blind with `fail_detail[-2000:]` pasted in — destroys lineage. ⟨r4⟩ Note the distinction from this design: one process per unit of work is the same *shape*, but `--resume` plus drawer lineage is exactly what `wiggum` lacked. Rewrite, don't patch. |
| Deterministic scheduler as top tier | Cannot itself wedge, but there is nothing to talk to, no cross-repo judgment, and supervision degrades to crude timeouts. The Lead is an interface, not just a dispatcher. |
| Two peer Leads (Daisy's shape) | Correct at 8–10 concurrent projects; premature here. A cron one-liner buys the same resilience at v1 scale. |
| Three tiers from the start | At v1 volume the middle tier mostly relays, while adding a process that can wedge, a message hop, and a place for context to go lossy. |
| Strict Bash allowlist for ICs | Gates are arbitrary per-repo commands; the allowlist cannot be enumerated in advance and would block legitimate work constantly. |
| Container per IC | `xcodebuild` needs the macOS host, so it can't be universal; two execution paths where the weaker serves the iOS repo. |
| ⟨r2⟩ Gates-only merge authority (rev 1) | Tests-green ≠ review-clean. A dependency bump to a vulnerable version, a semantics-changing "mechanical" rename, or a docs change asserting something false all pass gates. |
| ⟨r2⟩ Claude fresh-context reviewer | Viable, and one fewer harness dependency — but same-model reviewer and implementer share blind spots. Codex gives genuine cross-model adversarial review at near-zero marginal cost given `launcher`. Reversible: it is one registry id. |
| ⟨r4⟩ Persistent IC session (`--input-format stream-json`) | A genuinely long-lived process driven over stdin, and the obvious reading of "long-lived IC". Rejected: it makes crash recovery a special case instead of the normal path, hides spend until the session ends, cannot be bounded by `--max-budget-usd`, and buys nothing that `--resume` does not already provide at ~5% marginal cost. |
| ⟨r4⟩ `--bg` background agents as ICs | They do persist for weeks — four were live on the test machine, the oldest from July. But they do not appear in `ListAgents` as addressable peers and there is no CLI to drive one, so the Lead could start them and never steer them. |
| ⟨r4⟩ Push messaging as the control channel | Now that it demonstrably works, tempting. Rejected: delivery lands only between the IC's tool calls, so latency is unbounded in the case that matters (an IC deep in a long gate run), and it is silently contingent on the receiver's permission mode. Fine for "stop"; wrong for "here is your work". |
| ⟨r5⟩ Pure goal loop (N unbounded) | Hand the whole issue to one `-p "/goal …"` invocation. Genuinely tempting: no turn runner at all, and evaluator-priced iteration. Rejected because the Lead then sees cost only at the end (violating Goal 6's "observable before the bill arrives"), cannot detect thrash — the evaluator asks "is the condition met?", never "is this failing the same way again?" — and cannot preempt on budget, priority, or an upstream close. |
| ⟨r5⟩ Pure re-invocation (N = 1, rev 4) | Not rejected — it is the N = 1 case and stays available. Rejected only as a *default*, because it spends one Opus Lead turn per IC turn, which can exceed the cost of the work being supervised. |
| ⟨r4⟩ Drawer-polling only, no push at all | Rev 3's fallback, and almost right. Rejected only because aborting an in-flight turn then degrades to `SIGKILL`, forfeiting that turn's spend and any uncheckpointed reasoning. Push earns its place on that one job. |

---

## Data flow

```
issue labeled `agent:ready`
   └─► Lead: pick by priority:* order; check budget, concurrency cap, per-issue attempt cap
        └─► Lead classifies risk (dispatch-time) → writes dispatch-state drawer
             └─► create git worktree for this issue
                  └─► Lead assigns session uuid; launcher spawns IC DISPATCH
                       │   (`-p "/goal <gates> or stop after N turns"`,
                       │    `--session-id` first time, `--resume` after)  ⟨r5⟩
                       ├─► IC reads knowledge base K: search / code_map / kg_query
                       ├─► IC reads lineage for THIS issue: prior attempts + verdicts
                       ├─► IC implements
                       ├─► IC runs approved gates for this repo
                       │     └─ fail ─► append attempt to lineage ─► bounded retry
                       │                 └─ retries exhausted ─► `agent:exhausted`
                       ├─► each turn: goal evaluator judges the gate condition
                       │     └─ not met ─► another turn, same process (up to N)
                       ├─► dispatch ends: IC checkpoints, process EXITS
                       │     └─► Lead banks `total_cost_usd`, decides, re-invokes
                       │          next dispatch on the same session uuid  ⟨r5⟩
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

⟨r9⟩ **Everything below the `green` branch of this diagram is executed by `autopilot advance`, not by the Lead tick.** Rung 10; see *Who merges*. Before it, the arc was drawn but never run.

⟨r2⟩ Note that **every attempt appends a lineage record — successes included**, not only failures. The record shape always carried `commit_sha`; rev 1's diagram only wrote on failure, which would have left successful approaches unrecorded and therefore re-derivable.

### Storage — validated against the live MCP surface

**No new MCP tools are required for v1.** Confirmed by reading the tool schemas, not assumed.

| What | Where | Shape |
|---|---|---|
| Knowledge base (K) | existing `search`, `code_map_load`, `kg_query` | unchanged |
| Attempt lineage | new drawer room `backlog-lineage` | **append-only**: one drawer per attempt, `{issue, attempt_n, approach, verdict, why_failed, commit_sha}` |
| Per-issue current state | same room | **`logical_key`** per issue: best-so-far, cumulative `attempt_n` across dispatches |
| ⟨r2⟩ Dispatch state | same room | **`logical_key`** per in-flight issue; the Lead's crash-safe memory. ⟨r4⟩ carries the **assigned session uuid** and `turn_n`, so any Lead can resume any IC |
| ⟨r2⟩ Daily budget ledger | same room | **`logical_key`** per date. ⟨r4⟩ accumulated from each invocation's `total_cost_usd`, not from `token_usage` |
| Exact issue→attempt traversal | `kg_add` / `kg_query` | triples `issue-283 --has_attempt--> <attempt_id>` |
| Approved gate config | drawer, `logical_key` per repo | inferred + human-approved, `pending` → `approved` |

> **Design hazard — do not use `logical_key` for attempt records.** `add_drawer`'s `logical_key` *rewrites* the drawer in that wing/room. Applying it to attempts would silently overwrite each attempt with the next and destroy exactly the history this feature exists to keep. Attempts are plain `add_drawer` calls with no `logical_key`; only *status* drawers use one. This needs a regression test (see Testing).

Semantic `search` alone cannot reliably enumerate "every attempt on issue #283" — hence the `kg` edges for exact traversal.

### Budget accounting — the gap closed ⟨r4⟩

Verified against `crates/ironmem/src/hook.rs`:

- **Good news:** `persist_transcript_tokens` is *not* collab-specific. It runs on `stop`/`precompact`, is registry-driven via `HarnessSpec::transcript_parser`, and is gated only by `metrics_enabled()` — so headless IC sessions are in scope in principle.
- **It is best-effort by design** (`hook.rs:451` — "warns on failure, never fails the hook"), so a parse or upsert failure silently under-counts spend.
- **It refuses incomplete data:** the Claude parser rejects any transcript lacking a terminal `result` event (`metrics/transcript.rs:108`). **A watchdog-killed or crashed IC therefore contributes zero recorded tokens** — the sessions most likely to have burned budget are precisely the ones invisible to `token_usage`.

⟨r3⟩ **EMPIRICALLY BROKEN — measured 2026-08-23.** The live database (`~/.ironrace-memory/memory.sqlite3`, 525 MB) contains **zero `source='transcript'` rows**. Not a reduced count — none, ever. Only `mcp_response` (11,754), `llm_rerank` (10), and `pref_extract` (10) are present. This is not the feature being switched off: `metrics_enabled()` defaults to true and `IRONMEM_METRICS` is unset, so the path is enabled, implemented, hook-registered — and has never once persisted a row.

A likely contributing cause is visible in `~/.claude/settings.json`: the `Stop` hook is wrapped in `timeout --kill-after=2 5` and ends `2>/dev/null || true` — a **5-second cap with every error silenced**. `PreCompact`, by contrast, is given 60s. Parsing a large transcript within 5s is optimistic, and the silencing means no failure was ever surfaced.

⟨r4⟩ **This is no longer on the critical path.** The re-invocation primitive supplies a better meter than the hook ever would: `claude -p --output-format json` returns `total_cost_usd` and a full `usage` breakdown **for every invocation**, synchronously, in the Lead's own hands. The Lead's ledger is the sum of those values — not an estimate, not hook-dependent, not silently lossy.

Restated:

- **Authoritative meter:** the sum of `total_cost_usd` across IC and Reviewer invocations, written to the daily ledger drawer as each **dispatch** returns. ⟨r5⟩
- **Hard per-dispatch ceiling:** `--max-budget-usd`, enforced by the CLI rather than observed after the fact. ⟨r5⟩ At N > 1 this becomes a per-issue-attempt ceiling, which is the economically meaningful unit anyway.
- **Residual loss is one dispatch, not one session.** ⟨r5⟩ A killed process forfeits only the in-flight dispatch's accounting; everything already returned was banked. Coarser than rev 4's one-turn granularity, still bounded — and the bound is N turns of spend, which is exactly what N is choosing.
- ⟨r5⟩ **Evaluator spend is a separate, small line.** It runs on the small fast model and is billed there; see *Model routing*.
- **`token_usage` and transcript ingestion are demoted to hygiene.** Repairing them remains worth a separate issue — the metrics surface is broken for every other consumer too — but Autopilot no longer blocks on it, and no longer needs it even for reconciliation.

### Secret handling on the lineage write path ⟨r2⟩

Lineage records embed gate output — test failures, stack traces, command stderr — which can contain tokens, credentials, and environment values. **No write-time redaction exists today.** Verified: `sanitize.rs::sanitize_content` only validates non-emptiness and length; `search/sanitizer.rs` handles *query* degradation; `config.rs::redacts_sensitive_content` is a **read-time output mode** governing what MCP returns, not what is stored. Content written to a drawer is persisted verbatim.

The lineage writer must therefore truncate and scrub gate output before persisting. This is **new work, not reuse.**

### Extension point (v2, AVO)

The lineage store is shaped so the AVO loop is additive: add a `score` field to the attempt record and a scoring function per issue class, then change the commit rule from *"correctness passes"* to *"correctness passes **and** score ≥ best-so-far."* No storage migration, no change to the tiering, no change to the transport.

---

## Error handling

| Condition | Behavior |
|---|---|
| ⟨r5⟩ IC process exits at end of dispatch | **Normal, not a fault.** The Lead banks the dispatch's cost and re-invokes on the same session uuid. |
| ⟨r5⟩ Goal evaluator returns **impossible** | The goal clears and the invocation ends — a normal-looking exit. ⟨r6⟩ **Mechanism, measured:** the base result JSON carries no verdict field — `terminal_reason`/`stop_reason`/`subtype`/`is_error` are identical for a met and an impossible dispatch. The Lead distinguishes them via the turn-prompt template's required `--json-schema`, reading `structured_output.verdict`. A dispatch with no `structured_output` field (schema not honored, e.g. an infrastructure failure mid-turn) is never treated as met. |
| ⟨r5⟩ Goal condition and approved gates disagree | Cannot occur by construction: the condition is generated from the gate config. A test guards it. |
| ⟨r5⟩ Loop halts on the no-tool-use anti-stall | Returns control with the goal still set. To the Lead this is a short dispatch; it counts as an attempt and the next dispatch resumes normally. |
| ⟨r5⟩ Auth failure, exhausted credits, unrecoverable context overflow, or model unavailable | Clears the goal mid-dispatch. Distinguish from a completed dispatch; these are infrastructure failures, never attempts, and must not consume the per-issue attempt cap. |
| IC process dies mid-turn | process-health detects it; the next `--resume` continues the session from its last checkpoint. Lineage in drawers is unaffected; one turn's accounting is lost. |
| ⟨r2⟩ IC alive but mid-long-turn (ping unanswered) | **Not** treated as dead. Death requires unanswered ping **and** no checkpoint advance in the longer window. ⟨r4⟩ Reinforced by measurement: `-p` sessions expose **no `status` field** in the session registry, so busy/idle is simply unavailable for an IC. Checkpoint advancement is the only progress signal there is. |
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
| ⟨r5⟩ Killed IC's tokens never recorded | Bounded to the **in-flight dispatch only** — every completed dispatch was banked from its result JSON as it returned. |
| ⟨r4⟩ Turn exceeds its spend ceiling | `--max-budget-usd` terminates it; the Lead treats it as a failed attempt and appends to lineage. |
| ⟨r4⟩ Lead must abort a turn already in flight | Push a stop message (bypass mode makes it deliverable). It lands at the IC's next tool boundary, letting it checkpoint and exit cleanly. `SIGKILL` only if the push is not honoured within a bounded wait. |
| ⟨r4⟩ IC parked longer than the 1h cache TTL | Next turn pays a full context rebuild. Expected and priced, not an error — but the Lead should not park ICs across the boundary casually. |
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
| ⟨r4⟩ Dispatch N+1 resumed by a fresh process sees dispatch N's context | The session primitive actually persists across processes |
| ⟨r5⟩ The `/goal` condition is generated from the approved gate config, never authored separately | One definition of "done" |
| ⟨r5⟩ An evaluator verdict of *impossible* is recorded as a failure, not a completion | The normal-looking-exit trap |
| ⟨r6⟩ Dispatch invoked without a valid `structured_output.verdict` (missing, malformed, or schema not honored) is never recorded as met | Guards the mechanism 6a's fix depends on — base JSON alone gives no signal, so its absence must fail closed |
| ⟨r5⟩ A dispatch cleared by auth failure or credit exhaustion does not consume the per-issue attempt cap | Infrastructure failure is not an attempt |
| ⟨r5⟩ N = 1 reproduces rev-4 behaviour exactly | The parameterisation is a generalisation, not a replacement |
| ⟨r4⟩ Ledger total equals the sum of per-invocation `total_cost_usd` | Budget meter is exact, and independent of the metrics hook |
| ⟨r4⟩ Turn exceeding `--max-budget-usd` is terminated and recorded as a failed attempt | Hard spend ceiling is enforced, not merely observed |
| ⟨r4⟩ Abort message to a bypass-mode IC is honoured at its next tool boundary | The interrupt channel works, and its latency assumption holds |
| ⟨r4⟩ Abort message to a **non**-bypass IC is NOT delivered | Guards the coupling: dropping skip-permissions silently removes the interrupt channel |
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
- ⟨r4⟩ Budget accounting is now exact per completed turn, but still loses the in-flight turn when a process is killed. Bounded, no longer unbounded.
- ⟨r4⟩ The system is coupled to Claude Code CLI surface area — `--session-id`, `--resume`, `--output-format json`, `--max-budget-usd`, `claude agents --json` — measured against v2.1.241. These are stable-looking flags, not a versioned API contract, and a change to any of them is a change to this design.
- ⟨r2⟩ Secret scrubbing on the lineage path is net-new code on a path that persists raw command output. A bug there writes credentials into long-lived memory.
- Lineage drawers grow without bound absent an actively maintained retention policy.
- Per-repo gate approval is real friction before any repo becomes eligible.
- A second execution philosophy alongside `collab`/`iron-build` — two systems to keep coherent, and a standing temptation to converge them badly later.
- The `agent:ready` label is manual: forget to apply it and nothing gets worked. Autonomy is capped by a human remembering to opt issues in.

**Now committed to**
- ⟨r5⟩ The IC session primitive: **one process per dispatch of N turns**, `claude -p "/goal …" --resume` against a Lead-assigned session uuid, with rev 4's one-turn form as the N = 1 case. Control is pull; push is interrupts only.
- ⟨r5⟩ Model routing across four slots, and the rule that the goal evaluator is never upgraded to compensate for a soft condition.
- ⟨r4⟩ Per-invocation result JSON as the authoritative budget meter, with `--max-budget-usd` as the per-turn ceiling.
- The harness-native mesh for discovery and for the abort path.
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

⟨r4⟩ Three validation rounds have now run. Round 1 (2026-08-23) produced one blocker and one reversal-in-waiting; round 2 (2026-08-23) reversed it and retired the blocker; ⟨r6⟩ round 3 (2026-08-24, rung 0 of the build ladder) measured the ⟨r5-doc⟩ `/goal` claims for real and closed open question 6a.

**Resolved by validation**

1. ✅ **Headless addressability — WORKS.** Discovery works (`ListAgents`, and `claude agents --json` without a TTY). Delivery works **when the receiver runs in bypass-permissions mode**, which ICs do by design. ⟨r4⟩ Rev 3's "delivery refuted" was a probe artifact and has been corrected; see *Transport*.
2. ✅ **What is the long-lived IC session primitive? — SETTLED.** A **supervised re-invocation loop**: one `claude -p --resume <uuid>` process per **dispatch of N turns** against a Lead-assigned session id, with `/goal` driving the turns inside. `--session-id`/`--resume` were measured to restore context across separate processes at ~5% of first-turn cost inside the 1-hour cache TTL. ⟨r5⟩ Rev 4's one-turn form is the N = 1 case. See *IC lifecycle*.
3. ✅ **Is Lead→IC coordination push or pull? — SETTLED: both, with distinct jobs.** Pull (the next dispatch's prompt) is the control channel, because it is guaranteed-read and has no delivery dependency. Push is reserved for aborting a turn already in flight, because its latency is bounded by the IC's tool-call cadence and can never be relied on for assignment.
4. ⚠️ **Checkpoint cadence — RE-OPENED BY REV 5, then closed by instruction.** Rev 4 got it free: the turn boundary *was* the process boundary. At N > 1 intra-dispatch turns are evaluator-driven, so the IC must be **explicitly instructed** to checkpoint every turn. Settled in substance — the IC checkpoints per turn — but it is now a prompt requirement rather than a structural guarantee, and can therefore be got wrong.
5. ⚠️ **Transcript token ingestion is non-functioning** — zero `source='transcript'` rows in the live DB. ⟨r4⟩ **Demoted from prerequisite to hygiene.** Autopilot now meters spend from each invocation's result JSON and does not depend on the hook, even for reconciliation. Still worth its own issue, because the metrics surface is broken for every other consumer.
6. ✅ ⟨r6⟩ **Does the result JSON distinguish "met" from "impossible"? — CLOSED, negative, with a working mitigation.** Measured 2026-08-24: the base result JSON does not — `terminal_reason`, `stop_reason`, `subtype`, and `is_error` were byte-identical between a satisfiable and a deliberately unsatisfiable condition in a controlled A/B. **Fix:** the turn-prompt template now requires `--json-schema` with an enum `verdict` field (`met`/`impossible`/`not_met`); tested, and both arms produced a correctly-distinguishing `structured_output.verdict`. This was the one unverified load-bearing fact in rev 5, and it did not hold without the fix — see *6a, closed* under *IC lifecycle*.

**Still open, and worth deciding before implementation**

> **Approved 2026-08-23 at rev 4.** These three were explicitly *not* resolved by the approval — the design is approved, the tuning below is not yet decided. Each changes the shape of an implementation plan, so settle them before `/iron-plan` rather than during it.

7. ⚠️ **Per-dispatch wall-clock bound, and N itself. — HALF CLOSED ⟨r7⟩.** ⟨r9⟩ The *reviewer* is now bounded too (`review::REVIEW_TIMEOUT`, 1,200s, own process group): rung 5 shipped it unbounded, which was survivable while a human typed `autopilot review` and could interrupt it, and rung 10 runs it unattended where a wedged `codex` stalls every issue behind it. A fixed constant rather than a per-repo value, because a review reads a diff and runs no gate suite. **This is the fifth time an existing subprocess needed a bound a later rung had already learned to add.** The **wall-clock bound is closed**, in the form this entry itself recommends: `GateConfig.wall_clock_timeout_secs`, a per-repo `Option<u64>` with **no global default**, enforced by `run_dispatch_bounded` (own process group, `killpg` on timeout). Absence means unbounded and every path says so. **N is still unmeasured, and a fourth rung deferred it** — it needs real paid dispatches against a heavy gate, which is a spend decision, not a build task. The original text follows.

   ⟨r5⟩ Two numbers, not one. The dispatch needs a timeout after which it is considered wedged and killed — distinct from `--max-budget-usd` (spend) and `--max-turns` (hard iteration cap). And **N**, the turns per dispatch, trades supervision granularity against Lead spend; 5–8 is a starting suggestion, not a measured figure. ⚠️ ⟨r6⟩ **Partially addressed, not settled.** Rung 0's dispatches were lightweight (a file write, one real but fast gate command; 8–20s wall-clock, 1–5 turns) — nowhere near a real gate suite (`cargo test --workspace`, `xcodebuild`, a Python suite), so no responsible timeout constant can be derived from them; inventing one now would repeat the single-arm-probe mistake this spec's own method notes warn against. **Recommend:** make the wall-clock bound a **per-repo config value** (part of the same approved gate config the `/goal` condition is generated from), not a single global constant — repos' gate suites vary by an order of magnitude or more. N stays at the unchanged 5–8 suggestion; nothing rung 0 measured argues for a different number, because rung 0's tasks were too light to observe the thrash-timing tradeoff that actually determines it. Both need real-workload measurement, which is rung 4's job, not rung 0's.
8. ✅ ⟨r6⟩ **What the Lead actually puts in a turn prompt. — ANSWERED.** See the *turn-prompt template* under *IC lifecycle*: issue + lineage + active strategy redirect + worktree/push constraints + explicit per-turn checkpoint instruction + the gate condition (generated from the repo's approved config, never authored separately) + the schema-validated verdict requirement. Built and run for real against this repo's own `cargo fmt` gate across two `--resume`-joined dispatches; see the cost figures there.
9. ✅ ⟨r8⟩ **Whether the Lead is a Claude session at all. — CLOSED by rungs 7 and 8, and its residual BUILT by rung 9: a Rust-native mechanical supervisor, with three isolated one-shot calls.** Rev 6 narrowed this and left it open "pending rung 4/8 implementation experience", naming the two responsibilities that could still tip it back toward a persistent Claude session: **cross-repo prioritization** and **thrash detection**. Both have now been built.

   Thrash detection is rung 7's `autopilot/supervise.rs`: two elapsed-time clocks ANDed together, a normalized string comparison over the trailing run of failed attempts, and a four-row precedence table. Cross-repo prioritization is rung 8's `autopilot/queue.rs`: seven guards over exact values — a label string, an approval flag, an integer attempt count, a float against a float, a set membership test — and a four-key sort over immutable data. Neither contains a step where a language model would be reading anything but its own guess.

   What remains genuinely judgment-shaped is the three steps rev 6 named, and after rungs 7 and 8 each is smaller and better-bounded than rev 6 assumed:

   | Step | Where it is now ⟨r8⟩ | Why it is not mechanical |
   |---|---|---|
   | Dispatch-time risk classification | `lead::resolve_class` — the `risk:*` label wins outright; with no label it asks `advise::advise_risk_class`, and **every** other outcome falls back to `unclassified`, which fails closed at `decide_merge`'s `ClassMismatch` | Reading an issue's prose to route it |
   | Composing a strategy redirect | Rung 7's mechanical text is the floor; `advise::advise_strategy_redirect` may **append** a proposed alternative, never substitute one | It can name a repeated failure and forbid repeating it; it cannot propose a better approach |
   | Drafting a human escalation question | `lead::notify_escalation` posts a mechanical notice on the issue and adds `advise::advise_human_question`'s draft when there is one | Naming *what* is unclear is the judgment |

   **The decisive property is not that the three calls turned out to be unnecessary — it is that the loop does not depend on them being available.** A Lead that never makes one still runs: every issue dispatches as `unclassified` and therefore cannot auto-merge, redirects take rung 7's mechanical form, and no questions are asked. That is exactly what a cron-restarted supervisor needs, and it is what a long-lived Claude session — which can wedge, and whose context the watchdog's restart erases — cannot offer. Rung 8 implements the Lead as `lead::lead_tick`: one bounded pass, no resident state, `ironmem autopilot lead`.

   ✅ ⟨r8⟩ **Rung 9 builds all three**, in `autopilot/advise.rs`: a toolless, one-turn, schema-constrained `claude -p` call with no session and — uniquely in this subsystem — **no `--dangerously-skip-permissions`**, since a call that cannot use a tool needs no permission. Bounded three ways: `--max-budget-usd` per call, the same daily dollar predicate `run_issue` applies, and a per-day call ceiling. Priced in dollars, because unlike the Codex reviewer this CLI reports them.

   **Off by default** (`autopilot lead --advisor`), and the property that matters is not that the calls are good but that **the loop does not depend on them**: disabled, refused, unreachable, non-zero, unparseable, unschema'd, out-of-enum and explicitly-declined all land on the same behaviour rung 8 had. Each schema carries an explicit declining member (`unclear` / `no_proposal` / `no_question`), because a constrained enum with no way to say "I don't know" converts uncertainty into a confident wrong answer — and for the risk class the confident wrong answer is the one that can auto-merge.

   Rung 9 also closes a gap rung 7 left: an escalation stopped the work and recorded it in a drawer, telling nobody. `lead::notify_escalation` now comments on the issue once per failure signature, names `autopilot supervise --clear-escalation` as the way out, and carries the `AUTOPILOT_COMMENT_MARKER` every Autopilot comment carries. That notice is mechanical and is posted whether or not the advisor is available.

   ⚠️ **Residual, stated not hidden.** No rung has yet run one of these calls against the live API — the argv is built from the CLI's own `--help` and the result envelope is the one rungs 0 and 2 measured, but the combination is unmeasured, so every parse degrades to "unavailable" rather than to a value. **N is still unmeasured, and a fifth rung has passed it by; it is a spend decision, not a build task.**

**Tuning, deferred to implementation**

10. **Name.** "Autopilot" is a working title.
11. Concurrency cap — initial `N` for in-flight ICs.
12. Daily token budget — the actual ceiling figure, and the per-turn `--max-budget-usd`.
13. ⟨r2⟩ Within-dispatch retry bound, thrash-detection threshold, and cross-dispatch per-issue attempt cap — three distinct numbers.
14. Retention policy specifics — when attempt records compact to a summary.
15. ⟨r2⟩ Whether ICs (not just the reviewer) should route to Codex for some task classes.
16. Whether the Lead reuses `evaluate-issue` (DIRECT/IRON/COLLAB/SPLIT scoring, and its mandatory split above 15 tasks) for decomposition, or classifies independently.

---

## Validation log

| Round | Date | Claim under test | Result |
|---|---|---|---|
| 1 | 2026-08-23 | Headless session is discoverable and addressable | Discovery ✅ / delivery ❌ (later found to be a probe artifact) |
| 1 | 2026-08-23 | `token_usage` records headless IC spend | ❌ zero `source='transcript'` rows, ever |
| 2 | 2026-08-23 | Push delivery depends on the receiver's permission mode | ✅ controlled A/B; bypass delivers in <10s, non-bypass never delivers |
| 2 | 2026-08-23 | `--session-id` + `--resume` restores context across processes | ✅ |
| 2 | 2026-08-23 | Resume is cheap enough for per-turn re-invocation | ✅ ~5% of first-turn cost within the 1h cache TTL |
| 2 | 2026-08-23 | `--output-format json` is a usable budget meter | ✅ exact `total_cost_usd` per invocation |
| 2 | 2026-08-23 | `claude agents --json` enumerates sessions without a TTY | ✅ |
| 2 | 2026-08-23 | `-p` sessions expose busy/idle status | ❌ liveness only |
| 2 | 2026-08-23 | `--bg` agents are drivable as ICs | ❌ not addressable peers |
| — | 2026-08-24 | `/goal` mechanics (loop-to-completion under `-p`, transcript-only evaluator, met/not-met/impossible verdicts, anti-stall, goal-clearing errors, 4,000-char condition) | 📄 documented pre-rung-0; loop-to-completion, met/impossible verdicts (via schema), and the condition-as-opening-directive behavior are now ✅ measured — see round 3 below. Anti-stall, goal-clearing-error, and 4,000-char-limit behavior were not separately probed this round |
| 3 | 2026-08-24 | Does the base result JSON distinguish "met" from "impossible"? | ❌ **no** — `terminal_reason`, `stop_reason`, `subtype`, `is_error` identical across a controlled A/B (satisfiable vs. deliberately unsatisfiable condition). See OQ6a |
| 3 | 2026-08-24 | Does `--json-schema` fix that? | ✅ forces a top-level `structured_output.verdict` (`met`/`impossible`/`not_met`), correct in both arms of the same A/B. Now required in the turn-prompt template |
| 3 | 2026-08-24 | `--resume` carries the turn-prompt template's own content (not just a planted secret) across separate processes | ✅ dispatch 2 recalled the real gate command, its exit code, and the checkpoint summary verbatim without re-running the gate |
| 3 | 2026-08-24 | Resume cost, reproduced against a real gate command | ✅ dispatch 1 (first invocation, 5 turns): $0.187. dispatch 2 (`--resume`, 1 turn): $0.036 — ~19% of dispatch 1 |
| 3 | 2026-08-24 | Goal evaluator (Haiku) cost is a separate, small, billed line | ✅ appears as its own `modelUsage` entry in every probe, $0.002–$0.021 per dispatch regardless of turn count |
| 3 | 2026-08-24 | Goal evaluator cache reuse across turns/dispatches | ❌ `cacheReadInputTokens` was 0 for the Haiku line in all six probes — each evaluation re-creates its cache; the "intra-dispatch iteration is cheap" claim holds in absolute terms ($0.02-ish) but not because of cache reuse |
| 3 | 2026-08-24 | Total rung-0 probe spend | 6 invocations, $0.644 total |

**Method note, worth keeping.** Both rev-3 errors share a root cause: a single-arm probe was treated as a conclusion. `success: true` was read as *delivered* when it meant *accepted*, and a negative result from a probe differing from production in one flag was written into the spec as a property of production. Round 2 was run as a controlled A/B for that reason.

⟨r5⟩ A second method note, from a different failure: rev 4 was written without reading the `/goal` documentation, and so committed to owning a turn loop that the harness already provides. Read the platform's own primitives before building one. Note also that everything marked ⟨r5-doc⟩ is documentation rather than measurement — a weaker class of evidence than ⟨r4⟩, and marked so it can be told apart.
