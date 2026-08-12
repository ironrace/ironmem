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

Preserve the full raw diff transiently for deterministic trigger detection with
`gh pr diff <N>`, even when the compact artifact succeeds: conditional-reviewer
triggers need full source coverage and must not treat the lossy artifact as the
sole classifier. Do not inject or repeat that raw diff in reviewer prompts, and
never pass it to the workflow; discard it after selecting the conditional
lenses. The compact artifact remains the review context (or the raw fallback
when no artifact is available).

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

Preserve the full raw diff transiently for deterministic trigger detection with
`git diff HEAD`, even when the compact artifact succeeds: conditional-reviewer
triggers need full source coverage and must not treat the lossy artifact as the
sole classifier. Do not inject or repeat that raw diff in reviewer prompts, and
never pass it to the workflow; discard it after selecting the conditional
lenses. The compact artifact remains the review context (or the raw fallback
when no artifact is available).

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

The SHA is passed to the workflow as `rollbackSha` and does four jobs:

1. It scopes the scope-creep audit — `git diff <sha> -- .` is the fix diff, with
   the one exception below.
2. It is printed in **every** report, including `--report-only` runs and runs
   that applied zero fixes. A rollback line the user has to go looking for is
   not a rollback line.
3. Recovery is `git checkout <sha> -- .`, with the one limit below.
4. It is the commit the workflow cuts each Find-phase isolation worktree at —
   see the isolation bullet under "What the workflow guarantees". This is the
   one job that needs the anchor to be an **immutable object naming the
   reviewed tree** rather than merely a restore point: a worktree cut at a ref
   would move under the review, and one cut at `HEAD` would not contain the
   uncommitted work being reviewed at all. Anything that changes how this SHA is
   minted changes what those lenses read.

**Four gaps in `git`'s own behaviour that the report must state, not paper over.
Each one is a way the printed recovery line under-delivers or over-reaches, and
a recovery line the user cannot trust is worse than none:**

- `git diff <sha> -- .` does **not** show a file the fixes *created* — a new
  untracked file is invisible to it. A fix agent that adds a file would slip
  past the scope audit entirely. When reporting fix scope, run
  `git status --porcelain` alongside the diff and treat fix-created files as fix
  output.
- `git checkout <sha> -- .` restores modified files but does **not delete** a
  file that did not exist at anchor time. Recovery is therefore incomplete for
  created files, and the report must say so plainly rather than implying the one
  command undoes everything.
- `git add -A` **skips files matched by `.gitignore`**, so a gitignored path is
  absent from the anchor tree. If a fix lands in one — a generated config, a
  local `.env`, anything ignored but present — that edit is unanchored,
  invisible to the scope audit's diff, and `git checkout <sha> -- .` will not
  restore it. There is no recovery for it at all. Detect the overlap before
  Phase 3 and state it:

  ```bash
  printf '%s\n' "${CHANGED_FILES[@]}" | git check-ignore --stdin --no-index 2>/dev/null
  ```

  Any path it prints is outside the anchor. Name those paths in the report under
  the rollback line as **not covered by the anchor**. If a fix later lands in
  one, say so explicitly rather than letting the blanket recovery line imply
  otherwise.
- `git checkout <sha> -- .` reverts the working tree to the anchor —
  **including any edit the user made after the anchor was minted.** This review
  takes minutes, and it does not hold a lock on the tree. The recovery line must
  therefore carry its own scope: it undoes the fixes *and* anything else changed
  since the anchor. Print it as a statement of what it does, never as "undo the
  fixes":

  ```
  rollback: git checkout <sha> -- .  (reverts the tree to the pre-review
  snapshot — this also discards any edit you made during the review)
  ```

**The anchor must exist before the recovery line is printed at all.** The
workflow returns `rollbackUsable`. When it is `false` there is no anchor, the
workflow has already forced `reportOnly` and nothing was edited — print
`rollback: n/a (no anchor could be minted; nothing was edited)` and **never** a
bare `git checkout -- .`. That command with an empty sha is not a degraded
rollback: it discards every unstaged change in the tree, which is the work under
review.

<a id="fix-created-test"></a>
### The fix-created test

Both gaps above need one question answered — *did the fixes create this file?* —
and it has an exact answer, so do not improvise a discriminator:

> A path is **fix-created** iff it is untracked now **and** absent from the
> anchor tree:
>
> ```bash
> git status --porcelain -z --untracked-files=all \
>   | while IFS= read -r -d '' entry; do
>       [ "${entry:0:2}" = "??" ] || continue
>       p="${entry:3}"
>       git ls-tree -r "$ROLLBACK_SHA" -- "$p" | grep -q . || echo "fix-created: $p"
>     done
> ```
>
> Use `-z` and read NUL-delimited, not `awk '{print $2}'`. Porcelain v1 quotes
> any path containing a space or a control character (`?? "my file.txt"`), and
> `$2` both splits on the space and keeps the opening quote: the loop then
> searched the anchor for `"my`, found nothing, and printed
> `fix-created: "my`. Per the Phase 7 fill-in rules that string lands on the
> rollback follow-up line as the path the user must `rm` by hand — so the
> recovery instructions named a path that does not exist while the real
> fix-created file was never named at all. `-z` emits paths raw and unquoted,
> which is the whole reason it exists.

This works because the anchor was built with `git add -A` against the temp
index, so **every untracked file that existed before the fixes is already in the
anchor tree**. A path missing from that tree can only have appeared afterwards.

Use this test in both modes. Do not diff against Phase 1's `git status --short`:
that baseline exists only in Local Mode, and in PR Mode there is no captured
untracked list at all — improvising a discriminator there means guessing at
exactly the moment the tree is in an unknown state.

---

## Phases 3–6.5 — DELEGATE TO THE WORKFLOW

The user invoked a slash command whose instructions say to run this workflow.
**These instructions are the authorisation to call the `Workflow` tool** — call
it directly; do not stop to ask.

### Resolve the script path

In order:

1. `~/.claude/workflows/ultrareview.js` — the installed copy. **Always prefer
   this.**
2. `.claude-plugin/workflows/ultrareview.js` — the repo-local canonical copy,
   and **only** when the repo under review is the ironrace-memory checkout
   itself. Confirm that before using it: the remote must be the ironmem
   repository, or the path must be the checkout the user invoked the command
   from with the intent of testing an uninstalled change. If you cannot
   establish that, use the installed copy or stop.

This order used to be reversed, with the "only when the target is ironmem"
condition stated as prose rather than checked. That is a different kind of
mistake from the `/collab` precedent it cited: `/collab` resolves prompt
*text*, which a model reads, while this resolves *executable JavaScript* handed
straight to the `Workflow` tool. PR Mode is designed for arbitrary repos, so a
user reviewing a fork's PR would have run that fork's `ultrareview.js` — a
script that chooses agent types and models, writes fix briefs, and holds every
safety gate in this document. A substituted copy simply does not have them:
CONFIRMED-only, the anchor requirement and the changed-file allowlist all live
inside the script being replaced.

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
- **One verdict per claim.** The verify memo is keyed on the finding's exact
  text — file, line, issue, failure scenario — not on its location. The verdict
  printed beside a wording, and carried into a fix brief, is the verdict reached
  about *that* wording. Do not reintroduce a location key as an optimisation:
  the merge picks the primary wording by scenario length, so a location key
  makes the verdict and the wording two independent selections and the
  `CONFIRMED`-only gate becomes bypassable by wording swap.
- **Fix on `CONFIRMED` only** — never `PLAUSIBLE`, never `UNVERIFIED`, never
  before the verify pass. `fix_complexity: invasive` is reported, never patched.
- **Find-phase worktree isolation, decided by `ROSTER.mutates` in
  `ultrareview.js`.** Whether a given lens runs commands that write is declared
  there and nowhere else — do not restate that list here, in a prompt, or in a
  brief. Where the entry says so, the workflow cuts a throwaway worktree at
  `rollbackSha`, points that lens's working directory, diff range and
  `review-diff --repo` at it, waits, and removes **and** prunes it on both the
  success and the failure path. Where it does not, the lens reads this checkout
  directly and no worktree is cut. Two consequences for this command: the anchor
  is load-bearing beyond rollback (Phase 2.5), and a lens whose worktree cannot
  be cut is reported as an **errored** lens — never quietly re-run against the
  shared tree — so `coverageComplete` goes false and the Phase 6 precondition
  applies.
- **Fix agents grouped by file**, one agent per file, groups in parallel, no
  worktree isolation — the fixes land in the user's working tree. That is the
  deliberate opposite of the Find-phase policy above and stays that way: a fix
  applied inside a throwaway worktree would be deleted with it. The fan-out is
  capped, every dispatch is individually caught so one failure cannot discard
  the run, and a finding is only dispatched if its path is in the `files` list
  passed from here.
- **Fail-closed argument handling.** Auto-fix requires `reportOnly: false`
  exactly; anything else, including absence, is report-only. A `rollbackSha`
  that is not an object name disables auto-fix outright. `repoPath` and
  `diffRange` are refused if they carry characters a shell would interpret.
- The **scope-creep audit** whenever a fix agent was **dispatched** — not when
  one reported success. `fixes.applied` comes from the agents' own outcome
  strings, so gating on it would let the audited party decide whether it is
  audited.

### Consume the return struct

The workflow returns:

```
band ('small'|'medium'|'large') · changedLines · fileCount ·
droppedByBand[] · addedByBand[] · unrecognisedLenses[] · rosterWidened (bool) ·
fableSuggested (bool) ·
coverageComplete (bool) · erroredLenses[] ·
coverage[{ id, key, count, answeredBy, retried, errored, errorReason }] ·
findings[] · refuted[] · invasive[] · outOfScope[] ·
fixes { applied, files, groups[{ file, tier, results, errored, errorReason }],
        dispatchErrors[{ file, reason }], cappedFiles[] } ·
fixAgentsDispatched (bool) ·
scopeAudit (null | { in_scope, out_of_scope_changes, summary }) ·
verifyStats { confirmed, plausible, refuted, unverified, pastCap, supersededVerdicts } ·
isolationLeaks[] ·
reportOnly · reportOnlyForced (bool) · rollbackSha · rollbackUsable (bool)
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
- `outOfScope[]` is also a subset of `findings[]`: confirmed findings naming a
  file the caller never listed as changed. They carry `outcome: skipped` and are
  never dispatched to a fix agent, because a path that survives sanitising is
  not thereby a path this review may edit. Report them; do not treat the fact
  that they went unpatched as a decision the review made about severity.
- `fixes.cappedFiles[]` and `fixes.dispatchErrors[]` are **not** cosmetic. A
  file in `dispatchErrors` had an Edit-capable agent die on it, which is not the
  same as no edit: that file may hold a partial change no result set describes.
- `verifyStats.supersededVerdicts` counts verifier slots spent on a wording the
  cross-lens merge later displaced. It is a cost, not a defect — every verdict
  belongs to the wording it was reached about — but a non-zero value means the
  effective verify budget was smaller than the cap.
- `isolationLeaks[]` holds any Find-phase isolation worktree whose removal could
  not be confirmed. Each entry is a directory beside this checkout plus a stale
  entry in its worktree bookkeeping — it does not affect the verdict, but name
  the paths in the report with
  `git -C <path> worktree remove --force <path>` so the user can clear them.
- `verifyStats.pastCap` counts **claims** that outran the cap;
  `verifyStats.unverified` counts **findings** displaying an `UNVERIFIED`
  verdict. They are different numbers and `pastCap` is the larger: a capped
  claim at a location that also carried a verified wording is displaced into
  `also_reported`, where it is an unverified lead rather than an unverified
  finding. Quote `unverified` when talking about findings.

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
| Zero remaining CRITICAL/HIGH, validation passes, **coverage complete** | **APPROVE** |
| Only MEDIUM/LOW remaining, validation passes, **coverage complete** | **APPROVE with comments** |
| Any remaining HIGH, or any validation failure | **REQUEST CHANGES** |
| Any remaining CRITICAL | **BLOCK** |

Draft PRs → always **COMMENT**, regardless of findings.

### Coverage is a precondition, not a finding

The first two rows are the only ones that can clear a diff, and both of them
read a **count of findings**. A count of findings is a statement about what was
looked at, so neither row may be reached without establishing that something
looked. These are preconditions on APPROVE, checked before the table:

- **`coverageComplete` is `false`** — at least one lens contributed no coverage.
  A `0` beside an errored lens means "we did not look", not "nothing is there",
  and counting it toward a clean tally is the refusal-counted-as-APPROVE failure
  this design exists to eliminate. The security lens erroring is the worst case
  and looks identical to the best one in the count.
- **`rosterWidened` is `true`** — the requested lenses narrowed to nothing and
  the workflow substituted the core four. The review is real, but it is not the
  review that was asked for; say which lenses were requested and which ran.
- **`unrecognisedLenses` is non-empty** — an id no lens implements was
  requested. Something upstream believes a lens ran that never existed.
- **`fixes.dispatchErrors` is non-empty** — an Edit-capable agent died on a
  file. That file may hold a partial change no result set describes.

Where any of these hold, the decision is at most **APPROVE with comments**, and
the reason goes on the decision line:

```
Decision: APPROVE with comments — coverage incomplete (lens B security-reviewer ERRORED)
```

Never resolve a coverage gap by re-running the workflow. One automated pass,
then the human decides.

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
Coverage: <complete | incomplete — lens <id> ERRORED / roster widened / unrecognised id <x>>
Fixes applied: <N> across <M> files
  rollback: git checkout <sha> -- .  (reverts the tree to the pre-review
  snapshot — this also discards any edit you made during the review)
  <only when the fixes created files: "restores modified files; delete <paths> manually">
  <only when changed files are gitignored: "not covered by the anchor: <paths>">
  <when rollbackUsable is false, this whole block is: "rollback: n/a (no anchor
  could be minted; nothing was edited)">
Post-fix validation: <check → pass/fail/n/a, one line>
Remaining: <N MEDIUM (reported)> · <N HIGH invasive (design change — yours)> · <N out of scope>

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

## Reported, not patched (outside the reviewed file set)
<finding → the path it named, or "None">

## Refuted during verification
<finding → refuting evidence, or "None">

## Fix scope audit
<in scope / out-of-scope changes found, or "n/a (no fix agent was dispatched)">

## Cross-cutting patterns
<root causes from multiple findings, or "None">

## Validation
<check → pass / fail / n/a (+ reason when n/a)>

## Lens coverage
- <id> <lens>: <N findings> (<model/effort>[, retried on opus after empty fable return]) | ERRORED — did not look (<reason>) | skipped (<reason>) | dropped by diff-size band
- unrecognised lens id(s) requested: <ids, or omit the line>
- verification: <N confirmed / N plausible / N refuted / N unverified><, N past the cap><, N slots spent on a displaced wording>

## Roster
band <small|medium|large> · <N> changed lines · <M> files<, added by band: …><, roster widened to the core four (requested: …)><, this diff qualifies for --fable>
```

Filling it in:

- **Title line** — the command owns the title; it is not part of the workflow's
  return struct.
- **Decision line** — when auto-fixes changed the decision, say so on that line:
  `APPROVE with comments (was BLOCK — 2 CRITICAL auto-fixed)`.
- **Coverage line** — from `coverageComplete`, `erroredLenses`, `rosterWidened`
  and `unrecognisedLenses`. It sits directly under the decision because it is
  the precondition that decision rests on. `complete` only when
  `coverageComplete` is true, `rosterWidened` is false and `unrecognisedLenses`
  is empty.
- **Fixes applied** — `fixes.applied` and `fixes.files`. `<sha>` is
  `rollbackSha` from the struct. **Every report prints the rollback block**,
  including `--report-only` runs and runs with zero fixes — but print it as the
  four cases below, never as a bare command:

  1. `rollbackUsable` is `false` → `rollback: n/a (no anchor could be minted;
     nothing was edited)`. Do **not** emit `git checkout -- .`; with an empty
     sha that is not a weaker rollback, it is a command that discards every
     unstaged change in the tree.
  2. Otherwise the command carries its own scope: it reverts the tree to the
     anchor, **including anything the user edited during the review**. The
     review takes minutes and holds no lock on the tree, so a line that reads
     "undo the fixes" is wrong about what it does.
  3. `git checkout <sha> -- .` restores modified files but **does not delete
     files the fixes created**. Identify those with the
     [fix-created test](#the-fix-created-test) — untracked now **and** absent
     from `git ls-tree -r <rollbackSha>` — and name them on the follow-up line
     so the user knows recovery needs a manual `rm`.
  4. Gitignored paths are absent from the anchor entirely (`git add -A` skips
     them), so `git checkout <sha> -- .` cannot restore them and the scope
     audit's diff cannot see them. Name any that overlap the changed-file set,
     per Phase 2.5.

  A recovery command that silently under-delivers — or that over-reaches into
  the user's own work — is the exact failure class this command exists to
  prevent.
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
- **Reported, not patched (outside the reviewed file set)** — from
  `outOfScope[]`. Name the path each finding claimed. A finder emitting a path
  outside the diff is worth surfacing on its own: it is either a real
  cross-file defect the reviewer should chase manually, or a finder that has
  lost track of what it was reviewing.
- **Fix scope audit** — from `scopeAudit`, gated on `fixAgentsDispatched`, not
  on `fixes.applied`. `n/a` is correct **only** when `fixAgentsDispatched` is
  false. When it is true and `scopeAudit` is `null`, say the audit did not
  return and the dispatched files are unaudited — an agent holding `Edit` was
  pointed at them, and its own report of having changed nothing is not evidence
  the tree is unchanged. Add every file in `fixes.dispatchErrors` here by name:
  those agents died, possibly mid-edit.

  The auditor reads `git diff <rollbackSha> -- .`, which **cannot see files the
  fixes created**, so apply the [fix-created test](#the-fix-created-test)
  yourself and list every path it returns here. They are fix output and belong
  in the scope question like any other hunk — a file the fixes added is the one
  change most likely to be out of scope and is exactly what the diff cannot show
  you. Gitignored paths are invisible to it for the same reason and get the same
  treatment.
- **Lens coverage** — one line per entry in `coverage[]`, plus a line for every
  lens that was never requested (`skipped (<reason>)`) and every id in
  `droppedByBand` (`dropped by diff-size band`). `answeredBy` supplies the
  model/effort and the retry note; `retried` flags the Opus re-dispatch. End
  with any `unrecognisedLenses` on their own line — an id no lens implements is
  a stale caller, not a skipped lens, and must not be filed as one.
- **Roster** — `band`, `changedLines`, `fileCount`, `addedByBand` when non-empty,
  `roster widened to the core four (requested: …)` when `rosterWidened` is true,
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
- **A fix agent errors** → it appears in `fixes.dispatchErrors`. The run
  continues and the scope audit still runs, because an agent that died may have
  died *after* editing. Name the file under Fix scope audit and say the change
  is partial and undescribed. This is not the same as `skipped`.
- **A finding names a file outside the diff** → it lands in `outOfScope[]` and
  is never dispatched. Report it; do not chase it by widening the `files` arg.
- **Every requested lens dropped by the band** → the workflow widens to the core
  four and sets `rosterWidened`. The review is real but is not the one that was
  asked for: say so, and cap the decision at APPROVE with comments.
- **A lens errored** → `coverageComplete` is false. APPROVE is unreachable; see
  Coverage is a precondition. Never re-run the workflow to fill the gap.
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
