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
gh pr view <N> --json additions,deletions   # changedLines = additions + deletions
```
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
`headRefOid`. If they differ, Phase 6.6 validation must be
`n/a (working tree ≠ PR head)` — never run tests on a different tree and present
the result as the PR's.

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
| `marketingAgentType` | does the project define `.claude/agents/marketing-copy-auditor.md`, or name its claim surfaces in `CLAUDE.md` / `.claude/docs/`? | that agent type as a string, else `''` — never guess a path |

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
> diff-size band itself (`< 200` changed lines → core only; `200–800` → core +
> triggered; `> 800` lines or `> 20` files → **full roster**) and reports both
> `droppedByBand` and `addedByBand`. Do not pre-filter by size here — the band
> must be one deterministic rule in one place.

---

## Phase 2.5 — ROLLBACK ANCHOR

**This must come before anything that can edit.**

```bash
git add -N . 2>/dev/null || true
ROLLBACK_SHA="$(git stash create)"
[ -z "$ROLLBACK_SHA" ] && ROLLBACK_SHA="$(git rev-parse HEAD)"
echo "$ROLLBACK_SHA"
```

`git stash create` writes a commit object recording the working tree and index
as they stand. It does **not** touch the working tree, the stash list, or
`HEAD` — it only mints the object and prints its SHA. On a clean tree it prints
nothing, hence the `HEAD` fallback.

The SHA is passed to the workflow as `rollbackSha` and does three jobs:

1. It scopes the scope-creep audit — `git diff <sha> -- .` is exactly the fix
   diff and nothing else.
2. It is printed in **every** report, including `--report-only` runs and runs
   that applied zero fixes. A rollback line the user has to go looking for is
   not a rollback line.
3. Recovery is `git checkout <sha> -- .`.

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
| `reportOnly` | boolean | `--report-only` present |
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
- The **adversarial verify pass**: one verifier per surviving CRITICAL/HIGH,
  capped at 8, with a `CRITICAL_RESERVE` of 3 slots that a HIGH may not claim so
  a late-arriving CRITICAL still gets verified. Anything past the cap comes back
  `UNVERIFIED`. A `REFUTED` verdict requires quoting the specific guard or
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

---

## Phase 7 — REPORT

Output **inline only**. Do NOT write files. Do NOT post to GitHub. Do NOT write
to `.claude/PRPs/reviews/`.

```
# Ultra Review (local): <PR #N | Local> — <TITLE>

Decision: APPROVE | APPROVE with comments | REQUEST CHANGES | BLOCK | COMMENT (draft)
Fixes applied: <N> across <M> files · rollback: git checkout <sha> -- .
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
  `no_change_needed` belongs here with its `outcome_note`. **No tier** — no fix
  agent ran on these, and a tier here would name a model that never touched the
  file.
- **Reported, not patched (invasive)** — from `invasive[]`. **No tier**, for the
  same reason: `invasive` findings are never dispatched to a fix agent at all.
- **Refuted during verification** — from `refuted[]`, each with its
  `verification.evidence`, so the signal is not silently lost.
- **Fix scope audit** — from `scopeAudit`. `null` with fixes applied is not the
  same as no audit being needed: say the audit did not return and the fixes are
  unaudited.
- **Lens coverage** — one line per entry in `coverage[]`, plus a line for every
  lens that was never requested (`skipped (<reason>)`) and every id in
  `droppedByBand` (`dropped by diff-size band`). `answeredBy` supplies the
  model/effort and the retry note; `retried` flags the Opus re-dispatch.
- **Roster** — `band`, `changedLines`, `fileCount`, `addedByBand` when non-empty,
  and `this diff qualifies for --fable` when `fableSuggested` is true. Suggest
  the escalation; never auto-spend it.
- If `verifyStats.unverified > 0`, say so in the Summary — more CRITICAL/HIGH
  than the verify cap is itself a signal.

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

- **No `gh` CLI** → PR mode falls back to `git diff origin/<base>...HEAD`. Warn.
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
- **`git stash create` fails** → fall back to `git rev-parse HEAD`. If that
  fails too, force `--report-only` and say why in the report. Never fix without
  a rollback anchor.
- **Validation script missing** → record as `n/a`, don't block.
- **Working tree not at PR head** → validation `n/a` with reason; the lenses
  still review via the diff range.
- **Merge conflicts in PR** → surface under Validation as a HARD fail; still run
  the review.
- **`pr-review-toolkit` plugin not installed/enabled** → `toolkitAvailable:
  false`; the workflow runs lenses F–I on `general-purpose` with the same
  briefs, so the review degrades gracefully instead of erroring. Same fallback
  via `perfAgentAvailable` for lens K.
- **Verification budget** → if more than 8 CRITICAL/HIGH survive synthesis, that
  is itself a signal. The workflow verifies within the cap and tags the rest
  `UNVERIFIED`; those are never fixed. Say so in the Summary.
- **A lens returns empty** → under `--fable`, an empty or errored Fable lens is
  re-dispatched once on Opus and Lens Coverage records which model answered. A
  non-Fable empty return is not retried. A lens that **errored** is never
  rendered as `0 findings`.
