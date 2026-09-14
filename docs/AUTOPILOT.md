# Autopilot

Autopilot is the only IronRace subsystem that can merge to `main`
unattended. This is the operator guide: which command to run, in what
order, and what a label authorizes. For the design rationale behind these
decisions, see
`docs/iron/specs/2026-08-21-autonomous-backlog-runner-design.md`; for the
implementation, see `crates/ironmem/src/autopilot/`.

## What Autopilot is

Each tick, the Lead dispatches a single IC (implementer) into its own git
worktree against the repo's approved gate command, and records the result.
By default (`max_dispatches_per_tick = 1`) that is **one issue per tick
across every configured repo combined, not one per repo** — a five-repo Lead
still starts exactly one issue per tick unless an operator raises the limit.
Count issues, not model calls: the limit is applied as
`plan.dispatch.iter().take(max_dispatches_per_tick)`, and the single
`run_issue` it admits may spend several paid attempts inside that one tick.
The queue that slot is drawn from sorts on `resuming` first and `priority:*`
(highest first) second (`sort_candidates`), so a resumed in-flight issue
outranks every new candidate — but priority still orders the resumed issues
among themselves, not only the new work competing for the slot.
`ironmem autopilot advance` then finishes the job: it opens the pull
request the IC's push never does on its own, reviews the diff with a
fresh-context reviewer, and applies the merge decision that review
produces — merge, or hold the PR for a human.

## The two-command loop

Run these two commands, in this order:

```
ironmem autopilot lead --repo owner/repo=/path/to/checkout ...
ironmem autopilot advance --repo owner/repo=/path/to/checkout ...
```

`lead` reconciles in-flight work, un-blocks any `agent:blocked` issue that
has a newer human answer, supervises what is already running, and dispatches
the next issue off the queue. It starts work; it does not finish it. An IC's
dispatch ends with a push to its branch and nothing else — no PR is opened,
no review runs, no merge decision is made.

`advance` is the other half of that same loop: it opens the pull request for
any issue whose dispatch succeeded, reviews it with a fresh-context Codex
reviewer, and applies rung 5's merge decision. Running `advance` before
`lead` has nothing new to advance; running `lead` without ever running
`advance` leaves every successful dispatch sitting as a pushed branch with
no open PR, forever.

`ironmem autopilot queue --repo owner/repo ...` is a free, read-only
preview: it shows what the Lead would dispatch next — the same ordering and
the same budget/concurrency/attempt-cap checks `lead` applies — without
touching anything. Run it any time to see what a `lead` tick would do before
running one.

## Onboarding a repo

Onboard a repo before Autopilot can work its issues:

```
ironmem autopilot onboard owner/repo --path /path/to/checkout
ironmem autopilot approve owner/repo
ironmem autopilot labels owner/repo
```

`onboard` inspects the checkout's build manifests and CI configuration and
writes a **pending** gate-config proposal; it does not take effect on its
own. `approve` is the separate, explicit step that confirms it. Run them in
one sitting: a pending proposal that never gets approved leaves the repo
unable to dispatch anything, and a proposal approved long after it was
written may no longer match the checkout it was inferred from.

The gate itself is inferred, not authored by hand: which stacks a repo has
is decided by root-level build manifests (`Cargo.toml`, `package.json`,
`Makefile`, ...). **Only Rust's gate consults CI.** For a Rust repo,
`onboard` looks at the repo's own CI config to decide whether a format or
lint check is enforced at all, and takes CI's own command wherever it can be
run as written at the repo root. Where it can't — a command that isn't
plainly runnable, one CI runs outside the repo root, or two workflows
running the same tool differently — `onboard` falls back to a canonical
guess, and **always reports that on the proposal**, because the guess may be
stricter or looser than what CI actually enforces. A check CI doesn't
require to pass, or only uses to rewrite the tree, gets no gate command at
all. This is CI-*informed*, not CI-*equivalent*: a repo's first live
Autopilot run met an approved gate of `cargo test --workspace` and then
failed CI twice on `cargo fmt` and `cargo clippy`, neither of which the gate
mentioned.

**Read the pending proposal's warnings before running `approve`** — that is
the point at which a human catches an inference the CI config didn't
support.

Every other stack is inferred from the manifest alone and reads no CI config
at all: Python markers propose `pytest`, `Package.swift` proposes
`swift test`, a `package.json` with a real `scripts.test` proposes
`npm test`, and a makefile with a `test` target proposes `make test`
(`infer_python`, `infer_swift`, `infer_node`, `infer_makefile_fallback` —
only `infer_rust` is passed the CI evidence). Those gates are **test
commands only**: no format or lint check is proposed for them, and none is
warned about either. On a non-Rust repo, a proposal with no lint warnings
means no lint coverage — not that CI declined to require it.

`labels` creates the three `agent:*` labels (`agent:ready`, `agent:blocked`,
`agent:exhausted`) in the repo if they don't already exist. It is safe to
run more than once; an existing label is left untouched.

## The label taxonomy

### `agent:*` — who may dispatch this issue

| Label | Set by | Cleared by |
|---|---|---|
| `agent:ready` | A human opting an issue in; `lead` un-blocking an answered `agent:blocked` issue; `retry` forgiving an exhausted issue | Whenever **Autopilot** applies another `agent:*` label, which it always does as an exclusive transition (`plan_exclusive` adds the target and removes the other two). GitHub enforces nothing: labels added by hand coexist, and `agent:ready` alongside `agent:blocked` reads as **Blocked** (`blocked_beats_ready`) |
| `agent:blocked` | `ironmem autopilot ask`, when a human posts a question on an issue; also `advance`, when rung 6's merge decision holds a PR for a human (see *Human recovery paths* below). **`lead` never applies it** — it calls neither `ask_human` nor `exhaust_issue` | `lead`, on seeing a human comment newer than its own **question-marked** comment — flips back to `agent:ready` automatically. A merge hold posts no such marker and does **not** self-resume this way; see *Human recovery paths*. |
| `agent:exhausted` | Only `ironmem autopilot exhaust`, typed by a human. **Hitting the attempt cap does not apply it**: the issue keeps `agent:ready` and stays in every backlog listing, though `queue::plan_queue` defers it as `AttemptCapReached` before selection rather than dispatching it (`an_issue_at_its_attempt_cap_is_deferred_rather_than_dispatched`). It is stuck, not looping — and nothing on the issue says so (#345) | Only `ironmem autopilot retry` — never self-resumes |

An issue with none of these three labels is invisible to Autopilot: it is
not dispatched until a human adds `agent:ready`.

### `risk:*` — the eight classes, in two groups

`RiskClass::is_low_risk` splits the eight classes into exactly these two
groups, and nothing between them:

- **Eligible for auto-merge**, on green **and** a reviewer PASS:
  `documentation`, `dependency_bump`, `mechanical_rename`, `test_only`.
- **Always holds for a human**, regardless of reviewer verdict: `logic`,
  `protocol`, `security`, `public_api`.

**An issue carrying no `risk:*` label is `unclassified`, which fails
closed.** The merge decision compares the class the reviewer derives from
the diff against the class read from the issue's `risk:*` label — but only
once. That comparison is made, and frozen, the first time `advance` reviews
a given commit against a given base — and the label it freezes was read
earlier still. `advance` snapshots every issue's labels when it lists the
backlog at the *start* of the pass (`fetch_backlogs`, then `advance.rs:565`)
and hands that snapshot to `review_pr` as `dispatch_class`; the reviewer
never re-reads the issue. So in a pass carrying several issues, a `risk:*`
label removed while an earlier PR is being reviewed is still the label a
later PR in the same pass is measured against.
`unclassified` matches neither group at that point, so the comparison can
never succeed and the PR holds for a human.

**Relabeling the issue afterward does not revoke or grant merge
eligibility.** Every later `advance` pass against the same commit finds a
review already on file (`advance::next_step`'s `reviewed_this_head` check)
and goes straight to the merge decision without reviewing again;
`merge::evaluate` reads the class comparison off that *stored* review, not
off the issue's current label. Changing, removing, or adding a `risk:*`
label after the first review is a no-op until something forces `advance` to
review again and capture the label as it stands at that later moment. Two
things do, because `reviewed_this_head` matches on the head SHA **and** the
base branch: the IC pushes a new commit, or the PR is retargeted at a
different base. Retargeting an open PR — `main` to `release/1.x`, say —
re-reviews the same commit against the branch it would now land on, rather
than holding for ever on a review of a base that no longer applies
(`a_retargeted_pr_is_reviewed_again_rather_than_held_forever`).

Two exceptions. A review recorded before the base was stored at all has a
base of `None`, which matches any base, so a retarget does not re-review it.
And **retargeting *back* strands the PR**: `reviewed_this_head` asks whether
*any* stored review matches this head and base, while `merge::evaluate`
reads the *latest* review for the PR (`rfind`, no base filter). Review a
head against `main`, retarget to `release/1.x` (re-reviewed), then retarget
back to `main`, and `advance` finds the first review still matching and
skips the review, while the merge decision reads the `release/1.x` review
and holds at `BaseBranchMismatch` — with nothing left to re-review it. That
is the same never-recovers shape the base comparison was added to prevent,
reached by a second retarget instead of a first. Filed as #348. The cited
test covers only the first retarget.

## The auto-merge envelope

A PR auto-merges only when every one of these holds:

- The repo's approved gate is green.
- The fresh-context reviewer returns PASS.
- The diff's own risk class — as the reviewer classifies it, not as the
  Lead dispatched it — is one of the four low-risk classes above.
- The reviewer's classification of the diff matches the class stored on
  the review that `advance` recorded the first time it reviewed the PR's
  current commit against its current base (see above — captured once, from
  whatever `risk:*` label the issue carried at that moment, and not re-read
  from the issue on any later pass). A mismatch always holds for a human,
  even if both classes happen to be low-risk, because a diff that
  reclassifies itself mid-flight is exactly the case fail-closed exists for.

Everything touching `logic`, `protocol`, `security`, or `public_api` opens a
PR and waits for a human regardless of what the reviewer says. In the
`advance` workflow — the only path that runs unattended — applying a
`risk:*` label is the authorization for auto-merge: the dispatch class comes
from the issue's label by way of `advance::dispatch_class`, and no flag
grants it independently of the label.

**The two standalone subcommands are the exception**, and they are operator
tools rather than part of the unattended loop.
`ironmem autopilot review --class <class>` takes the dispatch class as a
*required argument* and never reads the issue's labels, and
`ironmem autopilot merge` then decides on the class that review stored
(`merge::evaluate` hands `review.dispatch_class` to `decide_merge`) without
reading a `risk:*` label either. So a human who runs
`review --class documentation` and then `merge` can merge a PR whose issue
carries no `risk:*` label at all. The label rule bounds what Autopilot may
merge on its own; it does not bound what an operator can authorize by
typing the command.

## What spends and what is irreversible

- `--merge` (on `ironmem autopilot advance`) is the one irreversible action
  in this subsystem — it executes `gh pr merge`. It is opt-in: without it,
  `advance` still opens PRs and runs reviews, but every merge is rehearsed,
  not executed.
- `--dry-run` (on `lead`, `advance`, `merge`, `exhaust`, `ask`) makes no
  GitHub writes — no `gh pr merge`, no label edit, no comment. That is not
  the same as writing nothing at all: `merge` still persists a rehearsal
  record to the local database (verified by
  `a_dry_run_is_recorded_as_a_dry_run`), tagged `dry_run: true` so it can
  never be read back as an executed merge. Nothing it records is visible
  outside Autopilot's own storage.
- `--advisor` (on `lead`) and the reviewer Codex invocation `advance` runs
  both spend real money. `--advisor` is off by default; every judgment it
  makes degrades to mechanical behavior (dispatch as the fallback class,
  keep the mechanical redirect text, escalate without a drafted question)
  when it's off, refused, or fails.
- The daily budget ceiling (`--daily-budget-usd`) tracks IC dispatches and
  advisor calls in one shared dollar ledger — but **it does not bound the
  reviewer**. Codex, unlike Claude, reports no `total_cost_usd` for a
  review, so every reviewer invocation is banked as *unpriced* spend
  (`unpriced_dispatch_count`) rather than added to the dollar total; a
  reviewer-only workload can run indefinitely under a `--daily-budget-usd`
  that never sees it move. What actually bounds the reviewer is
  `--max-unpriced-reviews-per-day` (default 20) — an invocation-count
  ceiling, not a spending ceiling. Do not rely on `--daily-budget-usd` to
  cap reviewer cost; size `--max-unpriced-reviews-per-day` instead.

## Human recovery paths

- **`agent:blocked`** resumes on its own *only* when a marked question was
  posted there, which today means `ironmem autopilot ask`: `lead` polls
  every such issue for a comment newer than that question, and flips it back
  to `agent:ready` the moment it finds one. A **merge hold** also sets
  `agent:blocked`, but posts no marked question, so this poll never resumes
  it — see *Branch protection* below for the manual recovery a merge hold
  actually needs.
- **A supervisor escalation is not this round trip, and sets no label at
  all.** When an issue's attempts keep failing the same way and a redirect
  fails the same way too, `lead` posts an escalation notice
  (`render_escalation_comment`) that carries Autopilot's comment marker but
  deliberately **not** the question marker, and flips nothing: the issue
  stays as it was. Replying to it does not resume the work — the notice
  names the command that does, `ironmem autopilot supervise owner/repo
  <issue> --clear-escalation`. The reasoning is in the code: what was
  escalated is an approach the supervisor has already proved does not
  converge, so an answer is not enough to restart it.
- **`agent:exhausted`** never self-resumes. The only escape is
  `ironmem autopilot retry owner/repo <issue>`, which **forgives** attempts
  made so far rather than zeroing the attempt counter — the counter also
  numbers the attempt history an IC's prompt is built from, so resetting it
  to zero would relabel that history rather than clear it. `retry` also
  flips the issue back to `agent:ready` unless `--no-label` is passed, and
  leaves a still-`agent:blocked` issue alone rather than pulling it back
  into the queue out from under an open question — **but only when
  `agent:exhausted` is absent.** `eligibility` returns `Exhausted` on sight
  of that label, before it ever considers `agent:blocked` (labels.rs:161),
  so an issue carrying both does not trip `retry`'s blocked guard and the
  exclusive move to `agent:ready` takes the blocking label with it. The same
  exclusivity runs the other way: `ask` moves an issue to `agent:blocked`,
  which clears `agent:exhausted`. Forgiving attempts and removing labels are
  separate acts, and only the first is what `retry` promises.
- **`ironmem autopilot ask`** and **`ironmem autopilot exhaust`** are not
  manual arms of something `lead` also does on its own: they are the *only*
  callers of `ask_human` and `exhaust_issue` outside tests. Blocking an
  issue on a question, and closing one out as exhausted, happen when an
  operator types those commands and at no other time.

## Branch protection

Where the PR's base branch requires an approving human review, the merge
holds as `HumanApprovalRequired`, exactly like any other held PR — Autopilot
is not the reviewer of record for that requirement and cannot satisfy it
itself. There is no bot bypass.

**A GitHub approval alone does not resume this.** Any merge hold — this one
included — sets `agent:blocked`, and `advance` only ever looks at
`agent:ready` issues; a blocked issue drops out of its backlog listing
entirely. `lead`'s auto-resume watches for a human *issue comment* posted
after one of Autopilot's own question comments (marked internally so it can
tell the two apart), and a merge hold posts a plain notice, not a marked
question — there is nothing for that check to find. Approving the PR on
GitHub, or commenting on the issue, changes nothing on its own. The only way
back is the manual step the hold comment itself names: re-label the issue
`agent:ready` once the approval (or whatever the hold required) is in place,
and the next `advance --merge` pass picks it up and re-evaluates it.
**Remove `agent:blocked` as you do it.** Adding `agent:ready` beside it
leaves both in place — nothing but Autopilot's own writes are exclusive —
and an issue carrying both still reads as Blocked, so it stays out of
`advance`'s listing exactly as before.
