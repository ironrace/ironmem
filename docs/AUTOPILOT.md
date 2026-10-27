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
has a newer human answer, supervises what is already running, dispatches
the next issue off the queue, and closes out any issue at its attempt cap
(a summary comment plus `agent:exhausted`). It starts work; it does not
finish it. An IC's
dispatch ends with a push to its branch and nothing else — no PR is opened,
no review runs, no merge decision is made.

`advance` is the other half of that same loop: it opens the pull request for
any issue whose dispatch succeeded, reviews it with a fresh-context reviewer
(Muse by default, or Codex with `--reviewer codex`), and applies rung 5's
merge decision. Running `advance` before
`lead` has nothing new to advance; running `lead` without ever running
`advance` leaves every successful dispatch sitting as a pushed branch with
no open PR, forever.

`ironmem autopilot queue --repo owner/repo ...` is a free, read-only
preview: it shows what the Lead would dispatch next — the same ordering and
the same budget/concurrency/attempt-cap checks `lead` applies — without
touching anything. Run it any time to see what a `lead` tick would do before
running one.

## Choose the implementation and review harnesses independently

Claude remains the default implementation harness, and an omitted
implementation model still resolves to `claude-sonnet-5`:

```
ironmem autopilot run owner/repo 364 --title "..."
ironmem autopilot lead --repo owner/repo=/path
```

Codex is explicit, including its model:

```
ironmem autopilot run owner/repo 364 --title "..." --implementer codex --model <codex-model>
ironmem autopilot lead --repo owner/repo=/path --implementer codex --model <codex-model>
```

A Codex implementer runs unattended with `--dangerously-bypass-approvals-and-sandbox`
and `--dangerously-bypass-hook-trust`, the same trust model as the Claude
implementer's `--dangerously-skip-permissions`: it executes as the operator,
with the operator's environment, on a prompt built from the issue title and
body. Codex documents both flags for externally sandboxed automation, so run
the Lead inside such a sandbox whenever `agent:ready` issues can be written or
edited by anyone you would not hand a shell. The hook-trust bypass also means a
project hook committed by one dispatch runs at the start of every later
dispatch or resume on that branch. Only the separate Codex *reviewer* runs
`-s read-only`.

Muse Code is explicit too. Pass the model you want, for example
`muse-wonder-1.0`:

```
ironmem autopilot run owner/repo 364 --title "..." --implementer muse --model <muse-model>
ironmem autopilot lead --repo owner/repo=/path --implementer muse --model <muse-model>
```

A Muse implementer runs `muse exec` unattended with the same trust model as the
Codex and Claude implementers: as the operator, with the operator's
environment. Autopilot passes:

- `--approval-mode never`: every tool call runs without a prompt.
- `--disable-sandbox`: Muse's own sandbox is off deliberately. Measured on
  1.4.13, it did not keep writes inside the workspace and it broke SSH commit
  signing.
- `--trust-workspace`: loads the workspace's rules, hooks and skills and
  enables agent delegation, so repository content steers the agent and runs
  code unattended. Run the Lead in an external sandbox as you would for Codex.
- `--disable-request-user-input`: the agent cannot stop to ask you a question.
- `--no-foreign-personal-context`: your personal Claude Code and Codex rules do
  not leak into an unattended run.
- `--prompt-file`: the prompt is a 0600 `prompt.md`, a sibling of the
  invocation's `schema.json`.

It also sets `MUSE_NO_AUTO_UPDATE=1`, so the launcher cannot swap binaries
between invocations.

An empty model is rejected for every harness. A known cross-harness prefix
is also rejected (Claude rejects `gpt-*` and `muse-*`, Codex rejects `claude-*`
and `muse-*`, Muse rejects `claude-*` and `gpt-*`), but Autopilot
does not impose a model allowlist: provider aliases such as a Claude `opus`
alias remain valid. A Codex or Muse implementation never resolves or launches
`claude`; `lead` also does not resolve the Claude advisor unless `--advisor`
is enabled.

The implementer choice is separate from `advance --reviewer muse|codex`.
The reviewer always gets a fresh invocation and review context, and its
accounting remains separate from implementation dispatch accounting:

| Implementer | Muse reviewer | Codex reviewer |
|---|---|---|
| Claude | Fresh Muse review; independent of the Claude implementation session | Fresh Codex review; independent of the Claude implementation session |
| Codex | Fresh Muse review; the Codex implementation does not replace or suppress it | Fresh Codex review with separate context/session and review accounting; using the same harness does **not** by itself guarantee cross-model independence |
| Muse | Fresh Muse review with separate context/session and review accounting; the same harness does **not** by itself guarantee cross-model independence | Fresh Codex review; independent of the Muse implementation session |

Different harnesses need not guarantee different underlying model families;
that depends on their provider configuration. The enforced property is fresh,
independent review context and accounting, not a claim about vendor model
lineage.

### Codex and Muse implementation bounds and accounting

Neither Codex nor Muse reports a price, so both are *unpriced* implementers.
An *invocation* is one launch of the Codex or Muse process.

The native Codex argv was verified against `codex-cli 0.163.0`. It has no
Autopilot-supplied session name or controller session ID, and it has no native
`--max-budget-usd` or `--max-turns` flags, so Codex refuses `--max-turns` too
(`--n-turns` still appears as prompt guidance). Muse has a native step cap but
no budget flag. So with either harness:

- `--max-budget-usd` is an error. Autopilot does not pretend the shared
  `RunConfig` field becomes native enforcement. Instead, each invocation
  pre-authorizes the default per-dispatch ceiling, $2.50, against the daily
  ledger.
- Every invocation has an enforced wall-clock bound: the repo's approved
  `autopilot timeout` value when configured, otherwise a 1200-second backend
  fallback.
- Each invocation reserves one unit of `--max-unpriced-dispatches-per-day`
  (default 5) before it launches, including an invocation that later exits
  nonzero, times out, or produces malformed output. For Codex and Muse that
  flag is the real daily dispatch cap. When it is reached the run stops as
  `UnpricedDispatchesExhausted`, paused rather than finished. An ambiguous
  crash in the narrow reservation-before-spawn window can conservatively
  consume a slot even when no child can later be proved.
- The ledger records no Codex or Muse dollars, so `--daily-budget-usd` does
  not bound them by dollars. It can still stop one: if the day's other priced
  spend (priced reviews, `--advisor` calls, Claude IC runs) plus the $2.50
  ceiling exceeds `--daily-budget-usd`, the run pauses as
  `DailyBudgetExhausted` (state `paused-daily-budget`) and the Lead's queue
  defers the issue. Neither names the flag. Only a `--daily-budget-usd` below
  $2.50 is refused at startup, with a message that names it.

Human output labels known spend and the unpriced dispatch count; JSON keeps
each Codex or Muse dispatch's `total_cost_usd` as `null`, the run total as the
known-dollar floor, and reports `unpriced_dispatches` separately. Unknown cost
is never rendered as free.

### Muse specifics

- `--max-turns` becomes `--max-model-steps`, and Autopilot passes
  `--workspace <worktree>`.
- Every invocation passes the controller's `--session-id`. Muse creates the
  session the first time and continues it headlessly after that, even after a
  kill, so a resumed run is the same conversation.
- The verdict is the `text` of the one `run_terminal` record on the requested
  session, shaped by `--output-schema`. Records on other session streams (for
  example delegated agents) are ignored.
- A missing terminal record (the process took a SIGTERM or SIGINT), or more
  than one on the requested session, is never success. Either way the run has
  no verdict, so it is an infrastructure failure: it consumes no attempt, and
  repeated ones escalate to you rather than retiring the issue. It is not a
  hold.
- What Muse does when a run exhausts `--max-model-steps` has not been
  measured live. By the code, any run without a valid verdict ends the same
  way: an infrastructure failure that consumes no attempt. Only if Muse exits
  0 with a `completed` terminal record carrying a schema-shaped verdict does
  that verdict count.
- Liveness uses the `ps` marker `--output-schema <.../schema.json>` on the
  `muse-bin-<version>` root (the launcher `exec`s it under the same PID), plus
  the shared process tracker. Each Muse shell tool is its own session leader,
  which the tracker follows by parent PID. Because those tools leave the
  root's process group, the group check below never sees them; the worktree
  check below is what catches a background child a short-lived tool leaves
  running.
- Muse's own lock refuses a second `exec` on a running session, and after a
  SIGKILL the lock is released (measured).
- `clear-dispatch` on a Muse hold deletes the dispatch state, so the next
  dispatch mints a new session UUID and starts a fresh Muse session. Lineage
  and attempt counts stand.

Restart recovery and holds otherwise follow the section below; hold and reason
messages name the harness or say "invocation".

### Codex and Muse identity, restart recovery, and supervision limits

This section is written for Codex. A Muse implementation recovers the same way
under the Muse markers and records above, and every `clear-dispatch` check
here applies to a Muse hold too.

Codex session identity comes only from the native JSONL
`thread.started.thread_id`. A resume uses that native UUID. Persisted state
also records its implementation harness; Claude state cannot be resumed as
Codex, or vice versa.

Public `run` and `lead` revalidate that persisted owner and Codex liveness
before provisioning or quarantining an issue worktree. Live or uncertain
process evidence therefore holds without moving, detaching, or replacing the
active checkout.

A Lead defers an issue whose state the other harness wrote
(`harness_mismatch`) instead of admitting it as a resume, so a leftover
state from a Claude Lead never takes a Codex Lead's dispatch slot, or the
reverse. Run the owning harness to finish it. An issue whose last run refused
because its goal condition was too long even after reduction is deferred
(`condition_too_long`) rather than resumed every tick; shorten the issue or
its gates, then release it with `autopilot clear-dispatch`.

Before spawn, Autopilot creates private schema, journal, final-message,
stderr, exit, and process-identity artifacts under the repository's absolute
common Git directory. That location survives worktree quarantine and lets a
replacement controller inspect the same evidence. Canonical artifact paths
containing whitespace or U+FFFD are currently refused: the `ps` evidence
parser cannot recover an argv boundary from whitespace, and it decodes
undecodable argv bytes from unrelated processes to U+FFFD.

Restart recovery is deliberately conservative. Missing or inconsistent
kernel identity, process evidence, or trusted supervisor completion produces
`Unknown` and holds the issue for inspection. Fresh authenticated live-process
evidence wins over a stale completion record. If `ps` is unavailable, only
Codex liveness becomes unknown; if the Claude registry source is unavailable,
only Claude liveness does.

A spawn that fails outright (for example, `codex` missing from `PATH`) proves
no child existed, so the supervisor records that, and the invocation is an
ordinary infrastructure failure rather than a hold. What stays held is
completion that really is unproven: a controller killed by Ctrl-C while
Codex kept running in its own process group, a root that exited before the
first `ps` sample, an observation error mid-run, a tool still running in
Codex's process group after Codex exited that no sample caught, or a process
started during the invocation still running in the worktree after cleanup
(see the worktree check below). Stop any Lead or `run`
working the repo, confirm nothing is still working on the issue, then release
it with:

```
ironmem autopilot clear-dispatch owner/repo 364
```

It refuses while a fresh process table shows anything that could be the
issue's work: the invocation's marker, another Autopilot marker for the same
issue, an ambiguous Codex command, or the recorded root PID, process group,
or a recorded descendant. A `codex exec` counts as ambiguous only when
Autopilot cannot parse its options *and* it names a `schema.json` file, which
is the only shape an Autopilot marker takes; your own `codex exec` runs with
other schema files never hold a dispatch. Once the supervisor has written its
completion record, a recorded PID no longer counts: cleanup already proved
that process gone, so the PID has been reused. Descendants are recorded as
the tracker enrols them, every `ps` sample (250 ms), so after a controller
crash the check also sees the tools that left the group with `setsid`,
including Muse's. It cannot see a tool started after the last sample before
the crash, or one that tool left running; that part of the confirmation is
yours. It also refuses a finished invocation that is only
waiting to be recorded (a Lead resuming the issue records it, as does
`autopilot run <repo> <issue> --implementer codex --model <model> --title
<title>`), unless lineage already holds that attempt, and it deletes nothing if
the state changes while it checks. It deletes only the dispatch state; lineage,
attempt counts, and the unpriced reservation stand, the artifacts stay on
disk for inspection, and the next dispatch starts a fresh Codex thread. A
Claude state clears only against a readable registry that does not list its
session.

You rarely need it for finished issues: each Lead tick (except a dry run)
first releases the leftover state of any issue that already succeeded or
reached its attempt cap, once nothing of it is running and its last
invocation is in lineage, so a crash between recording an outcome and
clearing its state cannot hold a concurrency slot for ever.

Direct native `codex` processes are the verified path. A launcher wrapper is
adopted conservatively after restart: a marker visible only on a different
native child is not treated as proof that the recorded root is the invocation.
The process tracker authenticates and cleans ordinary tool descendants,
including observed `setsid` children, but it is practical supervision rather
than hostile fork containment; a deliberate double-fork escape between polls
is outside its guarantee.

A tool that lives less than one sample and leaves a background child running
(`cmd &` in a short-lived shell) is the ordinary case of that escape: the
child is reparented to init before any sample can enrol it. Codex's tools
stay in its process group, so the group check catches it. Muse's do not, so
before writing the completion record the supervisor also refuses while any
process qualifies: its working directory is the worktree or inside it, you
own it, the kernel started it at or after the invocation's root, and it is
neither the supervisor nor the shared daemon (or one of its children). The
error names each one's PID and command, and the invocation holds its slot
like any other unproven completion. Your own shell or editor in the
worktree started earlier, so it never counts; one you open there during the
run does, and holds that run until you leave. macOS reads working
directories from the kernel (about 1.5 ms per completion for ~600
processes) and Linux from `/proc/<pid>/cwd`; on any other platform, or if
the scan fails, completion is refused. A child that `chdir`s out of the
worktree still escapes, and a deliberate escape is outside the guarantee.

The macOS runtime path is exercised. Linux-specific
kernel identity and cleanup code is covered by code/tests but has not been
run end to end on a Linux host.

Cleanup never kills the shared ironmem daemon your other sessions use. A
harness's `ironmem serve --connect` MCP server may autospawn that daemon as
its own child, but the process tracker never treats it as part of an
invocation. It recognises the daemon by its exact
`ironmem serve --listen <socket> [--db <path>]` command line, listening on
your configured daemon socket (`<state dir>/daemon.sock`, or
`IRONMEM_DAEMON_SOCKET`; the socket your registered MCP servers connect to),
on a process leading its own group, whose executable, as the kernel reports
it, is the same ironmem binary the controller runs. An agent-built
`target/debug/ironmem` with that command line is cleaned up like any tool,
and so is a daemon whose executable path cannot be read, or one started by
a different ironmem install than the controller's. It leaves that daemon's children alone
too, so an autospawned daemon survives the invocation and keeps serving. A
daemon on any other socket, such as one an ironmem test suite starts on a
temporary socket, is cleaned up like any other tool. So is a shared daemon
whose socket path contains whitespace, which the process table cannot
parse. The trade-off: a tool that runs your installed ironmem binary as the
daemon, with the same command line on your socket in a group of its own,
escapes tracking.
That is the same class as the double-fork escape above.

Verification records are intentionally separated:

- On 2026-10-22, native `codex-cli 0.163.0` new and resume **no-tools format
  probes** both exited 0, reused thread
  `01a16674-d173-7f53-b344-915194918f91`, and returned a valid `met` result.
  Those probes measured CLI format and resume identity; they were not
  implementation dispatches.
- A separate native timeout probe ran an actual tool child and verified that
  timed-out process-tree cleanup removed it. That was supervision validation,
  not the full implementation smoke described by a release/test report.
- On 2026-10-23, the public `autopilot run` command performed a full
  implementation dispatch through `/opt/homebrew/bin/codex` 0.163.0 with
  `gpt-6.1-sol`: it changed only `answer.txt` from `wrong` to `42`, passed the
  unchanged `make test` gate, committed and pushed the local issue branch,
  and returned strict `met`. JSON reported one dispatch with
  `total_cost_usd: null`, one unpriced dispatch, and a `$0.00` known floor.
  During the live window, public `reconcile` conservatively held the npm
  launcher/native-child identity twice rather than claiming `Adopt`; after
  trusted completion it returned an empty reconciliation and the dispatch
  state was clear. Logs and common-Git artifacts are preserved under the
  smoke directory named in the corresponding test report.

## Commit signing

This applies to every harness, Claude included. If `commit.gpgsign` is on, an
implementer that reports `met` (its "goal met" verdict) but whose commits are not
all signed has that run recorded as a failed attempt instead, because a model
can quietly pass `-c commit.gpgsign=false`.

- **What counts as unsigned.** A commit with no `gpgsig` or `gpgsig-sha256`
  header. `%G?` is only used to catch `B`, a bad signature.
- **Which commits are checked.** The whole issue branch: the agent's commits
  since the branch was cut, excluding anything already on another remote
  branch (`HEAD --not <cut> --exclude='*/<issue-branch>' --remotes`). The *cut*
  is the commit the issue branch was cut from. Commits merged or rebased in
  from a remote-tracking branch are not blamed on the agent, but local-only
  commits merged in are checked like the agent's own. An earlier dispatch's
  bypass stays caught after it is pushed.
- **The cut is fixed once.** `--base` is resolved to a commit sha in your
  checkout when the issue branch is created (or once, for an older branch that
  has no recorded cut), and stored as `branch.<branch>.ironmemCut`. A base
  that does not resolve is refused before any worktree exists.
- **The policy is sticky.** It is enforced if `commit.gpgsign` was on when the
  issue branch was created or at any run start (recorded as
  `branch.<branch>.ironmemSigning true`), or is on now.
- **Turning it off does not help.** If the agent, or anyone, turns
  `commit.gpgsign` off while `ironmemSigning` is set, the next `met` fails
  with "commit.gpgsign was turned off on this branch". Only an operator who
  unsets `ironmemSigning` with signing already off ends enforcement.
- **Trust posture.** The guard catches a lazy or accidental bypass. It does
  not stop an unconfined agent that deliberately signs with another key,
  clears the flag or the cut, or pushes its commits to another branch.

An unreadable policy, or a cut that `git log` cannot use when the `met` is
checked, fails closed as a failed attempt rather than a hold.

### A signing failure is a failed attempt

It counts toward the attempt cap and can end in `agent:exhausted`. The reason
the next turn and the operator see starts:

> commit.gpgsign is on, but commit(s) <shas> on this branch since its cut
> <sha> are unsigned or carry a bad signature. Re-sign them ...

Make sure the signing key or agent is reachable from wherever the Lead runs,
including inside an external sandbox. Otherwise every attempt fails.

### Stop the guard on one branch

The issue branch is named `autopilot/<owner>-<repo>-<issue>` (for `owner/repo`
issue 364, `autopilot/owner-repo-364`). To stop enforcing on it on purpose:

1. Turn `commit.gpgsign` off. The guard reads the effective value in the
   issue's worktree, so any config scope that applies there counts: repo,
   global or system.
2. In the repo, run `git config --unset branch.<branch>.ironmemSigning`.
3. Keep it off. Every run start records the flag again whenever the policy is
   on, so turning signing back on re-arms the guard.

Do these with no Lead or `run` active on the issue. A running attempt that
started with signing on still holds the branch to it, so it fails with
"commit.gpgsign was turned off on this branch".

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
| `agent:blocked` | `ironmem autopilot ask`, when a human posts a question on an issue; also `advance` (and `ironmem autopilot merge`), when rung 6's merge decision holds a PR for a human — **except** a PR held only for a missing approval, which keeps `agent:ready` (see *Branch protection* below). **`lead` never applies it** — it never calls `ask_human` | `lead`, on seeing a human comment newer than its own **question-marked** comment — flips back to `agent:ready` automatically. A merge hold posts no such marker and does **not** self-resume this way; see *Human recovery paths*. |
| `agent:exhausted` | `lead`, on an issue at its attempt cap: one whose run this tick ended `AttemptCapExhausted`, or one `queue::plan_queue` deferred as `AttemptCapReached` (the deferral is how a close-out whose label write failed is retried, and how an issue stranded at its cap is reached). An issue with a recorded success is skipped: it can only be at its cap through `--remediate`, and `advance` holds that PR for a human with `agent:blocked` instead. The close-out re-reads the issue's labels and leaves it alone if a human has since moved it to `agent:blocked` or removed `agent:ready`, and re-checks the cap so a `retry` granted mid-tick is honoured. It posts the summary comment, then applies the label; idempotent, so a repeat tick posts no second comment (`a_repeat_tick_on_an_exhausted_issue_posts_no_second_summary`, #345). Also `ironmem autopilot exhaust`, typed by a human | Only `ironmem autopilot retry` — never self-resumes. Re-labeling `agent:ready` by hand leaves the cap in force, so the next `lead` tick closes the issue out again |

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
- `--advisor` (on `lead`), implementation dispatches, and the reviewer
  invocation `advance` runs
  both spend real money. `--advisor` is off by default; every judgment it
  makes degrades to mechanical behavior (dispatch as the fallback class,
  keep the mechanical redirect text, escalate without a drafted question)
  when it's off, refused, or fails.
- The daily budget ceiling (`--daily-budget-usd`) tracks known-price IC
  dispatches and advisor calls in one shared dollar ledger — but **it does
  not bound an unpriced reviewer, or a Codex or Muse implementer, by
  dollars**. (A Codex or Muse dispatch can still pause when other priced
  spend plus the $2.50 default ceiling exceeds it; see *Codex and Muse
  implementation bounds and accounting*.) Muse and
  Codex reviewers report no `total_cost_usd`, so every reviewer invocation is
  banked as *unpriced* spend (`unpriced_review_count`) rather than added to
  the dollar total; a
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
  it: resolve what the hold names, then re-label the issue `agent:ready`
  (removing `agent:blocked`). The one merge hold that leaves an
  `agent:ready` issue's labels alone is a missing approval — see *Branch
  protection* below.
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
- **A Codex or Muse hold** (a dispatch that restart recovery could not prove
  finished) is released with `ironmem autopilot clear-dispatch owner/repo
  <issue>`, once you have stopped any Lead or `run` on the repo and confirmed
  nothing is still working on the issue. *Codex and Muse identity, restart
  recovery, and supervision limits* has the checks it makes.
- **A stored cut that is no longer a commit** stops the run with this error:

  ```
  branch.<branch>.ironmemCut is `<value>`, which is not a commit in <repo>; unset it with `git config --unset branch.<branch>.ironmemCut` to record the cut again from the base
  ```

  Run that `git config --unset` in the repo. The cut is recorded again from
  the `--base` of the next run, which may not be the original.
- **To stop the commit-signing guard on one branch**, follow *Stop the guard on
  one branch* under *Commit signing*.
- **`ironmem autopilot ask`** is not a manual arm of something `lead` also
  does on its own: it is the *only* caller of `ask_human` outside tests.
  Blocking an issue on a question happens when an operator types that
  command and at no other time. **`ironmem autopilot exhaust`** is
  different: `lead` closes out an issue at its attempt cap on its own, and
  the command is the manual arm of that same close-out.

## Branch protection

Where the PR's base branch requires an approving human review, the merge
holds as `HumanApprovalRequired` and the human is notified — Autopilot
is not the reviewer of record for that requirement and cannot satisfy it
itself. There is no bot bypass.

**Approving the pull request is all it takes to resume this.** This hold,
unlike every other merge hold, does not move the issue to `agent:blocked`:
the issue keeps `agent:ready`, so it stays in `advance`'s listing, and each
`advance --merge` pass re-reads the PR's `reviewDecision`. Once a human
approves, the next pass merges it — provided nothing else has changed, since
every other guard (the PR is open and not a draft, the review and the gate at
the current head, the base, mergeability) is still checked — with no label
edit and no comment needed. The hold comment is posted once per distinct hold;
while the approval is outstanding, later passes re-read the PR, say nothing
new, and do not spend the `--max-advances` burst limit on it. To take the
issue back instead, label it `agent:blocked`.

That holds only for an issue `advance` tracks: its repo's gate is approved
and Autopilot has recorded a successful attempt on it. A PR prepared by hand
and taken through `ironmem autopilot review` and `ironmem autopilot merge`
has no recorded success, so `advance` skips it as `NoSuccessYet` whatever its
labels. Its hold comment says so, and names the way back: re-run `ironmem
autopilot merge … --gate-green` once the PR is approved.

A human who **requests changes** is not an approval pending: GitHub then
reports `CHANGES_REQUESTED`, nothing in Autopilot will make the change asked
for, and the hold moves the issue to `agent:blocked` like any other, with a
fresh comment. A **transient** read — `mergeStateStatus: UNKNOWN` just after
the approval, or a protection endpoint that fails — is a different hold
(`PrNotMergeable`, `ProtectionUnknown`) and also sets `agent:blocked`, as
documented under `mergeable_state_permits`: re-label the issue `agent:ready`
once GitHub answers again.

(Before #346 this hold set `agent:blocked` like the others, and because
`agent:blocked` resumes only on a comment after a marked question, an
approval never resumed it. An issue parked that way by an older binary still
needs the manual step: make `agent:ready` its only `agent:*` label. `labels`
never rewrites an existing label, so a repo whose `agent:blocked` was created
earlier keeps the old description, which said it resumes on any newer human
comment.)
