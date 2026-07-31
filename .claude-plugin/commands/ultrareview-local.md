---
description: Deep local multi-agent review — correctness + code + security + architecture + docs + comments + tests + error-handling + type-design + concurrency, adversarially verified, verified findings auto-fixed, synthesized verdict (inline, no files, no GitHub writes)
argument-hint: "[pr-number | pr-url | blank] [--fable] [--report-only]"
---

# Ultra Review (Local)

Fans a roster of specialist review lenses out over one diff — code-reviewer,
security-reviewer, architect and doc-reviewer as the core, plus conditional
lenses when the diff matches their trigger (marketing-claims auditor,
comment-analyzer, pr-test-analyzer, silent-failure-hunter, type-design-analyzer,
concurrency-reviewer, performance-reviewer) — then adversarially verifies every
surviving CRITICAL/HIGH, **auto-fixes the confirmed ones**, validates the tree
after the fixes land, and decides. Use for large diffs, security-sensitive
changes, or architectural work. Stays entirely local — nothing written to disk
beyond the fixes themselves, nothing posted to GitHub.

The fan-out lives in a workflow script, not in this command. This file owns
every phase that needs a shell: fetch, context, trigger greps, the rollback
anchor, post-fix validation, decide, report.

**Input**: $ARGUMENTS

---

## Flags

| Flag | Effect |
|---|---|
| `--report-only` | Skip Phase 6.5. Findings are reported, never patched. |
| `--fable` | Run lenses A (correctness), C (architect), J (concurrency) on Fable 5 at `high` effort. **Never** Agent B (security). |

Strip both flags from `$ARGUMENTS` before parsing the remainder as a PR
number/URL. Either may appear in any position; neither takes a value. Pass the
result through as the `fable` and `reportOnly` booleans in Phase 3–6.5.

---

## Model policy

Per-lens model and effort are **declared in `ultrareview.js`** and applied by the
`Workflow` runtime. This command does not dispatch review agents itself and must
not override, restate, or second-guess those tiers — the roster table in the
script is the single source of truth for who runs on what.

- **Agent B (`security-reviewer`) runs on Opus permanently.** `--fable` never
  touches it, under any flag combination. Two reasons: Fable's documented
  bug-finding gains explicitly **exclude** security-focused analysis, and
  Fable's classifiers decline security-shaped briefs with
  `stop_reason: "refusal"` — HTTP **200** with empty content.
- Under `--fable`, an **empty or errored** lens is re-dispatched once on Opus,
  because a refusal and a clean pass are indistinguishable at the transport
  layer. Lens Coverage records which model produced each result. Empty returns
  from non-Fable lenses are **not** retried; terminal errors from them are still
  flagged as errors.
- `effort` only takes effect on the `Workflow` dispatch path — the plain `Agent`
  tool has no `effort` parameter. This command dispatches through `Workflow`, so
  the efforts declared in `ultrareview.js` are real.

---

## Mode

- `$ARGUMENTS` (flags stripped) contains a PR number / URL / branch → **PR Mode**
- Empty → **Local Mode** (uncommitted + staged + untracked changes on `HEAD`)

---

## Phase 1 — FETCH

**PR Mode:**
```bash
gh pr view <N> --json number,title,body,baseRefName,headRefName,headRefOid,changedFiles,additions,deletions,isDraft,mergeStateStatus
gh pr diff <N> --name-only
```
That single `gh pr view` already carries `additions` and `deletions` —
`changedLines = additions + deletions`. Do not issue a second call for them.
After resolving `<repo_path>`, `<baseRefName>`, and `<headRefName>`, attempt:

```bash
ironmem review-diff --repo <repo_path> --base <baseRefName> --head <headRefName>
```

Use its compact stdout as the review input **only on success**. On error,
unavailable feature, or a nonbeneficial artifact, discard its output and use the
exact existing raw fallback `gh pr diff <N>`. Do not retain the full raw diff as
the review input when the artifact succeeds. Diff range for the lenses:
`<baseRefName>...<headRefName>`. If PR not found → stop.

Preserve the full raw diff **transiently** for deterministic trigger detection
with `gh pr diff <N>`, even when the compact artifact succeeds:
conditional-reviewer triggers need full source coverage and must not treat the
lossy artifact as the sole classifier. Do not inject or repeat that raw diff in
reviewer prompts and never pass it to the workflow; discard it after selecting
the conditional lenses. The compact artifact remains the review context (or the
raw fallback when no artifact is available).

Record whether the local working tree is at the PR head: `git rev-parse HEAD` vs
`headRefOid`. If they differ:

- Phase 6.6 validation must be `n/a (working tree ≠ PR head)` — never run tests
  on a different tree and present the result as the PR's.
- **Force `reportOnly: true`.** The findings describe the code at
  `<base>...<head>`; the working tree holds different code, so a fix agent would
  edit files whose contents and line numbers do not match the finding it is
  answering. And because validation is `n/a` by construction on that tree, there
  is no gate left to catch the damage — an APPROVE carrying unvalidated edits to
  the wrong tree would be reachable. **Never edit a tree you cannot validate.**
  Say in the report that auto-fix was disabled and why.

**Local Mode:**
```bash
git status --short
git add -N . 2>/dev/null || true   # register untracked files as intent-to-add so new files appear in the diff
git diff HEAD --name-only
git diff HEAD --shortstat          # "N files changed, X insertions(+), Y deletions(-)" → changedLines = X + Y
```
First attempt:

```bash
ironmem review-diff --repo <repo_path> --worktree
```

Use its compact stdout as the review input **only on success**. On error,
unavailable feature, or a nonbeneficial artifact, discard its output and use the
exact existing raw fallback `git diff HEAD`. Do not retain the full raw diff as
the review input when the artifact succeeds. If empty → stop:
"Nothing to review." Diff range for the lenses: `HEAD`.

Preserve the full raw diff **transiently** for deterministic trigger detection
with `git diff HEAD`, on the same terms as PR Mode above.

Record title, file list, changed-line count, draft flag, and the selected review
input. The compact artifact's index supports exact source expansion:

```bash
# PR range form
ironmem review-diff --repo <repo_path> --base <baseRefName> --head <headRefName> --expand-file <path> --hunk <ordinal>
# Local Mode form
ironmem review-diff --repo <repo_path> --worktree --expand-file <path> --hunk <ordinal>
```

That exact string — placeholders `<path>` and `<ordinal>` left literal — is the
`expandCmd` handed to the workflow. When the raw fallback is in use there is no
index to expand, so `expandCmd` is the empty string.

If >50 files or >2000 additions, warn — but the parallel split scales.

### Capability probes

Three booleans/strings the workflow needs to resolve agent types. Probe them
here; the workflow has no filesystem access and cannot.

| Arg | Probe | Value |
|---|---|---|
| `toolkitAvailable` | is the `pr-review-toolkit` plugin installed and enabled (its `pr-review-toolkit:*` agent types available this session)? | `true` / `false` — when `false` the workflow runs F–I on `general-purpose` with the same briefs |
| `perfAgentAvailable` | is the `performance-optimizer` agent type available? | `true` / `false` — when `false` lens K falls back to `general-purpose` |
| `marketingAgentType` | see the three-way rule below | an agent type string, or `''` — never guess a path |

`marketingAgentType` has three cases, and they must not be collapsed — the
workflow uses a non-empty value both as the agent type **and** as the signal
that lens E is wanted at all:

1. `.claude/agents/marketing-copy-auditor.md` exists → pass that agent type
   (`marketing-copy-auditor`).
2. No such definition, but `CLAUDE.md` / `.claude/docs/` names the project's
   claim surfaces → pass `'general-purpose'`. The claim surfaces are real, so
   the lens has something to check; it just has no bespoke agent to check it
   with.
3. Neither → pass `''`. Lens E is skipped, and the `large` band will not pull it
   in either.

---

## Phase 2 — CONTEXT (keep it thin)

Read only what the agents won't:
1. Root `CLAUDE.md` and matching `.claude/docs/` rules
2. PR body (intent, linked issues, test plan)
3. Plan artifacts under `.claude/PRPs/plans/` or `docs/` matching the branch

Do **not** pre-read the full contents of changed files — the Phase 1 review
input plus paths is enough; let the agents inspect source independently.

The short summary you produce here is the `context` arg. Nothing found → `''`.

---

## Trigger detection

Grep the transient full raw diff (not the compact artifact) for conditional-lens
triggers, so every trigger sees complete source coverage. Do not inject or
retain that raw detection input afterwards; the compact artifact remains the
reviewer context. Expand an indexed file/hunk or inspect source directly when
detail is needed.

| Agent | Grep the diff for |
|---|---|
| comment-analyzer (F) | added/modified docstrings, `///`, `"""`, `/** … */`, or block comments beyond trivial one-liners |
| pr-test-analyzer (G) | any non-doc/config change to application logic (default ON unless diff is docs/config-only) |
| silent-failure-hunter (H) | `try`, `except`, `catch`, `unwrap_or`, `.ok()`, `.catch(`, `rescue`, `recover(`, retry/fallback logic |
| type-design-analyzer (I) | `class .*BaseModel`, `@dataclass`, `enum`, `struct`, `interface`, new domain types |
| concurrency-reviewer (J) | `async`, `await`, `spawn`, `thread`, `Mutex`, `RwLock`, `Arc<`, `atomic`, `BEGIN`/`COMMIT`/`transaction`, `UPDATE ... SET`, read-then-write on shared state, queue/channel ops |
| performance-reviewer (K) | queries inside loops, new DB queries without LIMIT/index, O(n²) scans over collections, allocation in hot loops, unbounded caches/collections |

Lens E (marketing-claims auditor) is project-gated, not grep-gated: include it
only when `marketingAgentType` resolved **and** the diff touches those claim
surfaces (marketing copy, pricing/plan config, feature flags, coverage/count
claims, JSON-LD).

The output of this section is `lenses`: the applicable core ids (`A`, `B`, `C`,
`D`) plus every conditional whose trigger fired.

> Pass every triggered lens id through to the workflow. The workflow applies the
> diff-size band itself — a small diff narrows to the core lenses, a large one
> expands to the full roster — and reports both `droppedByBand` and
> `addedByBand`. Do not pre-filter by size here, and do not restate the
> thresholds: the band must be one deterministic rule in one place, and
> `ultrareview.js` owns both the rule and its numbers.

---

## Phase 2.5 — ROLLBACK ANCHOR

**This must come before anything that can edit.**

```bash
TMP_INDEX="$(mktemp -u)"
cp "$(git rev-parse --git-path index)" "$TMP_INDEX"
GIT_INDEX_FILE="$TMP_INDEX" git add -A
ROLLBACK_SHA="$(git commit-tree "$(GIT_INDEX_FILE="$TMP_INDEX" git write-tree)" -p HEAD -m 'ultrareview rollback anchor')"
rm -f "$TMP_INDEX"
echo "$ROLLBACK_SHA"
```

This snapshots the working tree — **tracked modifications and untracked files
alike** — into a commit object, using a **throwaway copy of the index** so the
real one is never written. It touches nothing the user can see: not the index,
not the working tree, not the stash list, not `HEAD`, and it adds no stash
entry. `git commit-tree` succeeds on a clean tree too, so there is no
empty-output case to paper over.

**Do not use `git stash create` here.** Phase 1 Local Mode runs `git add -N .`
to make untracked files visible in the diff, and `git stash create` **fails** on
an index holding intent-to-add entries:

```
$ git stash create
error: Entry 'b.txt' not uptodate. Cannot merge.
Cannot save the current worktree state      # exit 1, and EMPTY stdout
```

Empty stdout is indistinguishable from the clean-tree case, so a naive
`[ -z "$SHA" ] && SHA=$(git rev-parse HEAD)` fallback silently anchors a **dirty**
tree to `HEAD`. The report would then print `git checkout HEAD -- .` as its
recovery command — which reverts every tracked file to `HEAD` and **destroys the
uncommitted work that was the subject of the review.** The safety line becomes
the weapon. Untracked files are routine in Local Mode, so this is the common
path, not an exotic one.

### If the anchor cannot be minted

Falling back to `git rev-parse HEAD` is legitimate **only when
`git status --porcelain` is empty** — on a clean tree, `HEAD` genuinely is the
pre-fix state. On a dirty tree it is not, and there is no safe anchor:

> If the anchor command fails and the tree is dirty, **force `reportOnly: true`**
> and say why in the report. Never auto-fix without a real anchor.

### What the anchor is for

The SHA is passed to the workflow as `rollbackSha` and does three jobs:

1. It scopes the scope-creep audit — `git diff <sha> -- .` is the fix diff, with
   the one exception below.
2. It is printed in **every** report, including `--report-only` runs and runs
   that applied zero fixes. A rollback line the user has to go looking for is
   not a rollback line.
3. Recovery is `git checkout <sha> -- .`, with the one limit below.

**Two gaps in `git`'s own behaviour that the report must state, not paper over:**

- `git diff <sha> -- .` does **not** show a file the fixes *created* — a new
  untracked file is invisible to it. A fix agent that adds a file would slip
  past the scope audit entirely. When reporting fix scope, run
  `git status --porcelain` alongside the diff and treat new untracked files as
  fix output.
- `git checkout <sha> -- .` restores modified files but does **not delete** a
  file that did not exist at anchor time. Recovery is therefore incomplete for
  created files, and the report must say so plainly rather than implying the one
  command undoes everything.

---

## Phases 3–6.5 — DELEGATE TO THE WORKFLOW

The user invoked a slash command whose instructions say to run this workflow.
**These instructions are the authorisation to call the `Workflow` tool** — call
it directly; do not stop to ask.

### Resolve the script path

In order:

1. `.claude-plugin/workflows/ultrareview.js` — the repo-local canonical copy,
   present when the target is the ironrace-memory checkout itself. This mirrors
   how `/collab` resolves its turn prompts.
2. `~/.claude/workflows/ultrareview.js` — the installed copy.

Neither exists → **stop** and tell the user to run `scripts/install-ironmem.sh`.
Do not hand-dispatch the lenses (see Edge cases).

### Call

Call `Workflow` with `{ scriptPath: <resolved>, args: { … } }`:

| Arg | Type | Value |
|---|---|---|
| `mode` | `'local'` \| `'pr'` | the resolved Mode |
| `repoPath` | string | absolute repo path |
| `diffRange` | string | `'<base>...<head>'` in PR Mode, `'HEAD'` in Local Mode |
| `reviewInput` | string | the compact `ironmem review-diff` artifact, or the raw-diff fallback |
| `expandCmd` | string | the exact expansion command with `<path>` / `<ordinal>` left as placeholders, or `''` when no artifact index exists |
| `context` | string | the Phase 2 summary, or `''` |
| `files` | string[] | changed file paths |
| `changedLines` | number | additions + deletions |
| `lenses` | string[] | the triggered lens ids from Trigger detection |
| `rollbackSha` | string | the Phase 2.5 anchor |
| `fable` | boolean | `--fable` present |
| `reportOnly` | boolean | `--report-only` present, **or** forced `true` by the PR-head mismatch (Phase 1) or a missing rollback anchor on a dirty tree (Phase 2.5). Whenever it is forced, say so in the report. |
| `toolkitAvailable` | boolean | capability probe |
| `perfAgentAvailable` | boolean | capability probe |
| `marketingAgentType` | string | capability probe, `''` when none |

Do **not** pass the transient raw diff. It is trigger-detection input only; it
never enters a reviewer prompt and it never enters `args`.

### What the workflow guarantees — do not duplicate it here

- **Per-lens model/effort tiers**, and the `--fable` swap plus the Opus retry.
- The **coverage-first output contract** in every finder brief: report
  everything found, including low-confidence and low-severity; no confidence
  floor, no word budget; failure scenario mandatory for CRITICAL/HIGH.
- **Phase 5 synthesis as code** — dedup by location, severity escalation across
  lenses, demotion of any CRITICAL/HIGH without a concrete failure scenario, and
  preservation of a displaced lens's wording in `also_reported`.
- The **adversarial verify pass**: one verifier per surviving CRITICAL/HIGH, up
  to the cap, with a reserve of slots a HIGH may not claim so a late-arriving
  CRITICAL still gets verified. Anything past the cap comes back `UNVERIFIED`.
  The cap and the reserve are declared in `ultrareview.js`; their values are
  deliberately not repeated here. A `REFUTED` verdict requires quoting the guard or
  invariant that prevents the failure — that clause is defined verbatim in
  `ultrareview.js` and is deliberately not restated here, so it cannot drift.
- **Fix on `CONFIRMED` only** — never `PLAUSIBLE`, never `UNVERIFIED`, never
  before the verify pass. `fix_complexity: invasive` is reported, never patched.
- **Fix agents grouped by file**, one agent per file, groups in parallel, no
  worktree isolation — the fixes land in the user's working tree.
- The **scope-creep audit** on the fix diff, when any fix was applied.

### Consume the return struct

The workflow returns:

```
band ('small'|'medium'|'large') · changedLines · fileCount · droppedByBand[] · addedByBand[] ·
fableSuggested (bool) ·
coverage[{ id, key, count, answeredBy, retried, errored, errorReason }] ·
findings[] · refuted[] · invasive[] ·
fixes { applied, files, groups[{ file, tier, results }] } ·
scopeAudit (null | { in_scope, out_of_scope_changes, summary }) ·
verifyStats { confirmed, plausible, refuted, unverified } ·
reportOnly
```

Each item in `findings[]` / `refuted[]` / `invasive[]` carries:

```
file, line, severity, confidence, issue, failure_scenario, suggested_fix,
lenses[], demoted?, also_reported[],
verification { verdict, evidence, fix_complexity, fix_class },
outcome?, outcome_note?
```

Phases 6 and 7 read **this struct only**. Do not re-derive findings, counts, or
verdicts from agent chatter, tool transcripts, or the workflow's log lines — the
struct is the record.

Notes on reading it:

- `line: 0` means a **file-level** finding, not line zero. Render it as
  `<file>` rather than `<file>:0`.
- MEDIUM/LOW findings carry `verification.verdict: 'N/A'` — they were never
  verified because only CRITICAL/HIGH are. Print no verification tag for them
  rather than inventing one.
- `demoted: true` means the finding was a CRITICAL/HIGH with no usable failure
  scenario and was dropped to MEDIUM before verification. Tag it `[demoted]`.
- `invasive[]` is a subset of `findings[]`. Those entries have no `outcome`, so
  they count as remaining **and** get their own report section.

---

## Phase 6.6 — VALIDATE (after fixes)

Numbered 6.6 because it runs after the workflow's Phase 6.5 fixes land. It
executes **before** Phase 6 DECIDE. Validation runs in this main loop, in the
real working tree, with real exit codes — a fix that breaks the test suite must
surface as a failure, not as a green review.

**PR Mode guard**: only run validation if the working tree is at the PR head
(checked in Phase 1). Otherwise record every check as
`n/a (working tree ≠ PR head)` — a green run on the wrong tree is worse than no
run.

Detect project type, run only what applies. Record the **real exit code** per
check. A missing script is `n/a`, not a failure.

- **Rust** (`Cargo.toml`): `cargo fmt --all -- --check` · `cargo clippy --workspace --all-targets --all-features -- -D warnings` · `cargo test --workspace`
- **Node/TS** (`package.json`): `npm run typecheck || npx tsc --noEmit` · `npm run lint` · `npm test`
- **Go** (`go.mod`): `go vet ./...` · `go test ./...` · `go build ./...`
- **Python** (`pyproject.toml`): `ruff check .` · `pytest`

If a check fails **and** `fixes.applied > 0`, say so explicitly: name the check,
state that fixes were applied before it ran, and print the rollback command
`git checkout <rollbackSha> -- .`. Never present a post-fix failure as
pre-existing without evidence — if you claim it pre-dates the fixes, show the
evidence (e.g. the same check failing at `<rollbackSha>`).

---

## Phase 6 — DECIDE

Decide on **remaining** findings only: those in `findings[]` whose `outcome` is
not `fixed`. Refuted findings are gone. Fixed findings are reported separately
and do not drive the decision.

| Condition | Decision |
|---|---|
| Zero remaining CRITICAL/HIGH, validation passes | **APPROVE** |
| Only MEDIUM/LOW remaining, validation passes | **APPROVE with comments** |
| Any remaining HIGH, or any validation failure | **REQUEST CHANGES** |
| Any remaining CRITICAL | **BLOCK** |

Draft PRs → always **COMMENT**, regardless of findings.

A post-fix validation failure **is** a validation failure and cannot be argued
down to APPROVE.

**Where those two rules meet:** "regardless of findings" means exactly that —
*findings*. A post-fix validation failure is not a finding; it is a fact about
the state of the tree. A draft run that applied fixes and then failed validation
still reports **COMMENT (draft)**, but the failure is carried on the decision
line and must never be allowed to read as clean:

```
Decision: COMMENT (draft) — post-fix validation FAILED (cargo test), see Validation
```

The draft rule governs the review posture. It does not suppress a broken tree.

**All checks `n/a` is not "validation passes."** Where the decide table says
"validation passes", it means at least one check ran and none failed. If every
check is `n/a` — no recognised project type, or a working tree that is not at
the PR head — say so on the decision line rather than treating the absence of a
failure as a pass. Note that the PR-head case cannot carry fixes at all, since
Phase 1 forces `reportOnly` there.

---

## Phase 7 — REPORT

Output **inline only**. Do NOT write files. Do NOT post to GitHub. Do NOT write
to `.claude/PRPs/reviews/`.

```
# Ultra Review (local): <PR #N | Local> — <TITLE>

Decision: APPROVE | APPROVE with comments | REQUEST CHANGES | BLOCK | COMMENT (draft)
Fixes applied: <N> across <M> files · rollback: git checkout <sha> -- .
  <only when the fixes created files: "restores modified files; delete <paths> manually">
Post-fix validation: <check → pass/fail/n/a, one line>
Remaining: <N MEDIUM (reported)> · <N HIGH invasive (design change — yours)>

## Summary
<2-3 sentences covering every lens that was dispatched>

## Fixed
<file> (<model/effort>)
  <line — issue — what changed [tags]>

## Remaining findings
CRITICAL: <file:line — issue — failure scenario — fix [tags] [CONFIRMED|PLAUSIBLE|UNVERIFIED]>
HIGH: ...
MEDIUM: ...
LOW: ...

## Reported, not patched (invasive)
<finding → why it needs a design decision, or "None">

## Refuted during verification
<finding → refuting evidence, or "None">

## Fix scope audit
<in scope / out-of-scope changes found, or "n/a (no fixes applied)">

## Cross-cutting patterns
<root causes from multiple findings, or "None">

## Validation
<check → pass / fail / n/a (+ reason when n/a)>

## Lens coverage
- <id> <lens>: <N findings> (<model/effort>[, retried on opus after empty fable return]) | ERRORED — did not look (<reason>) | skipped (<reason>) | dropped by diff-size band
- verification: <N confirmed / N plausible / N refuted / N unverified>

## Roster
band <small|medium|large> · <N> changed lines · <M> files<, added by band: …><, this diff qualifies for --fable>
```

Filling it in:

- **Title line** — the command owns the title; it is not part of the workflow's
  return struct.
- **Decision line** — when auto-fixes changed the decision, say so on that line:
  `APPROVE with comments (was BLOCK — 2 CRITICAL auto-fixed)`.
- **Fixes applied** — `fixes.applied` and `fixes.files`. `<sha>` is the Phase 2.5
  `rollbackSha`. **Every report prints the rollback line**, including
  `--report-only` runs and runs with zero fixes.

  `git checkout <sha> -- .` restores modified files but **does not delete files
  the fixes created**. If `git status --porcelain` shows untracked files that
  were not there at anchor time, name them on the follow-up line so the user
  knows recovery needs a manual `rm`. A recovery command that silently
  under-delivers is the exact failure class this command exists to prevent.
- **Fixed** — findings whose `outcome` is `fixed`, with the fix agent's
  `outcome_note` as "what changed". `no_change_needed` and `skipped` entries do
  not belong here. **Group them by file and label each file with the tier its
  fix agent ran at**: `fixes.groups[]` carries `{ file, tier, results }`, and
  `tier` is already formatted by the workflow as `model/effort` — render it
  directly, no lookup. The tier is per file by construction (one agent per
  file), so it belongs on the file heading, not repeated on every line.

  This is the one tier a reader most needs. Lens coverage records which model
  *found* each issue; this records which model *edited their working tree*, and
  this command's whole premise is that it edits. A fixed finding whose group
  cannot be located in `fixes.groups[]` gets `(tier unknown)` — never a guessed
  tier.
- **Remaining findings** — grouped by severity, tagged with `lenses[]`
  (e.g. `[A+B]`), `[demoted]` where set, the verification verdict for
  CRITICAL/HIGH, and any `also_reported[]` wording from another lens, which is
  preserved rather than discarded. A finding with `outcome: skipped` or
  `no_change_needed` belongs here with its `outcome_note`. **No tier** — the
  tier is a record of *an edit*, and no edit was made for these findings. That
  holds even for `skipped` / `no_change_needed`, whose file *was* dispatched to
  a fix agent at some tier: the agent ran and deliberately changed nothing, so
  printing its tier here would imply work that was not done.
- **Reported, not patched (invasive)** — from `invasive[]`. **No tier**, for the
  same reason: `invasive` findings are never dispatched to a fix agent at all.
- **Refuted during verification** — from `refuted[]`, each with its
  `verification.evidence`, so the signal is not silently lost.
- **Fix scope audit** — from `scopeAudit`. `null` with fixes applied is not the
  same as no audit being needed: say the audit did not return and the fixes are
  unaudited. The auditor reads `git diff <rollbackSha> -- .`, which **cannot see
  files the fixes created**, so check `git status --porcelain` yourself and list
  any new untracked files here. They are fix output and belong in the scope
  question like any other hunk.
- **Lens coverage** — one line per entry in `coverage[]`, plus a line for every
  lens that was never requested (`skipped (<reason>)`) and every id in
  `droppedByBand` (`dropped by diff-size band`). `answeredBy` supplies the
  model/effort and the retry note; `retried` flags the Opus re-dispatch.
- **Roster** — `band`, `changedLines`, `fileCount`, `addedByBand` when non-empty,
  and `this diff qualifies for --fable` when `fableSuggested` is true. Suggest
  the escalation; never auto-spend it.
- If `verifyStats.unverified > 0`, say so in the Summary — CRITICAL/HIGH
  findings that outran the verify cap are a signal in themselves. This number is
  also the honest one to quote: it counts findings whose verifier errored or
  returned nothing, not just those past the cap.

**An errored lens must never render as `0 findings`.** When
`coverage[].errored` is true, that lens contributed **no coverage** — a `0`
beside it means "we did not look", not "nothing is there", and rendering it as
a clean pass reproduces exactly the refusal-counted-as-APPROVE failure this
design exists to eliminate. Give it the visually distinct
`ERRORED — did not look (<errorReason>)` form shown above, and never let it
count toward a clean lens tally.

End with one line: the next step that fits the decision.

---

## Edge cases

- **No `gh` CLI** → PR mode falls back to `git diff origin/<base>...HEAD`. Warn,
  and source the rest of the PR data set from git rather than improvising it:
  `<base>` ← the repo's default branch (`git symbolic-ref refs/remotes/origin/HEAD`,
  else `main`); title ← the branch name; PR body ← unavailable, so Phase 2
  context is thinner and `context` says so; draft flag ← unknowable, so treat
  the PR as non-draft **and state that in the report** rather than silently
  assuming. The head check is satisfied by construction (the range ends at local
  `HEAD`), so validation runs normally and fixes are allowed.
- **Docs/config-only diff** → do not trigger the security or pr-test lenses;
  core code + architect + doc-reviewer still apply (doc-reviewer catches stale
  cross-references between docs). The `large` band may add them back — that is
  the band doing its job, and it is reported in `addedByBand`.
- **Diff <50 lines** → suggest `/code-review` instead; proceed only if the user
  confirmed.
- **`--report-only`** → confirmed findings appear under **Remaining**, not
  Fixed. Still print the rollback SHA.
- **Fix agent returns no result for a finding** → the workflow records it as
  `skipped` with a note. It counts as remaining and drives the decision. Never
  report it as fixed.
- **Post-fix validation fails** → report it as a failure, name the check, print
  the rollback command. Do **not** attempt a second fix round — one automated
  pass, then the human decides.
- **Scope audit finds out-of-scope changes** → surface them under Fix scope
  audit and recommend the rollback. Never silently accept an out-of-scope hunk.
- **Workflow script missing** → stop with the `scripts/install-ironmem.sh`
  instruction. Do **not** hand-dispatch the lenses: the per-lens tiers, the
  `CONFIRMED`-only fix gate, and the verify cap all live in the script, and a
  hand-rolled fan-out would fix on unverified findings.
- **The `Workflow` call itself fails** → it can die *mid-Fix, with edits already
  in the working tree*: the fix agents run in parallel and one rejecting can
  fail the run after its siblings have already edited files. You hold
  `rollbackSha`. Report the failure, print the rollback command, state plainly
  that the tree may hold **partial, unaudited, unvalidated fixes**, and emit
  **no decision** — there is no findings struct to decide on, and an APPROVE or
  BLOCK invented without one would be fabricated. Do not retry the workflow.
- **Rollback anchor cannot be minted** → legitimate only on a clean tree, where
  `git rev-parse HEAD` is a true pre-fix anchor. If `git status --porcelain` is
  non-empty, force `reportOnly: true` and say why. Never auto-fix a dirty tree
  without a real anchor.
- **Validation script missing** → record as `n/a`, don't block. Every check
  `n/a` is not a pass — say so on the decision line.
- **Working tree not at PR head** → validation `n/a` with reason, **and
  `reportOnly` is forced** so nothing is edited in a tree that cannot be
  validated. The lenses still review via the diff range.
- **Merge conflicts in PR** → surface under Validation as a HARD fail; still run
  the review.
- **`pr-review-toolkit` plugin not installed/enabled** → `toolkitAvailable:
  false`; the workflow runs lenses F–I on `general-purpose` with the same
  briefs, so the review degrades gracefully instead of erroring. Same fallback
  via `perfAgentAvailable` for lens K.
- **Verification budget** → more CRITICAL/HIGH surviving synthesis than the cap
  allows is itself a signal. The workflow verifies within the cap and tags the
  rest `UNVERIFIED`; those are never fixed. Say so in the Summary. Read the
  count off `verifyStats.unverified` rather than restating the cap.
- **A lens returns empty** → under `--fable`, an empty or errored Fable lens is
  re-dispatched once on Opus and Lens Coverage records which model answered. A
  non-Fable empty return is not retried. A lens that **errored** is never
  rendered as `0 findings`.
