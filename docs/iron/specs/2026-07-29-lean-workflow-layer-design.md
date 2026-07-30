# Lean Workflow Layer — Design

**Date:** 2026-07-29
**Status:** Approved design, pending implementation plan
**Scope:** Replace the eight vendored Superpowers skills with a self-contained, harness-parity workflow layer bundled in the ironmem plugin, add complexity-based model routing, and rework `/collab` onto the new layer.

---

## Problem

ironmem bundles eight skills vendored from [obra/superpowers](https://github.com/obra/superpowers) (`.claude-plugin/skills/ATTRIBUTION.md`). They total ~2,404 lines, and near-identical copies are hand-maintained in `.claude-plugin/skills/` and `.codex-plugin/skills/` — ~4,800 lines of duplicated authoring surface.

Three concrete defects follow from this:

1. **Silent behavioral drift between harnesses.** The two copies have diverged substantively, not just cosmetically. `.codex-plugin/skills/subagent-driven-development/SKILL.md:17-34` contains an 18-line "Controller-Owned Loop" section instructing the controller not to hand control back mid-plan. The Claude copy has no equivalent. Today, whether the implementer continues after task 1 depends on which harness is driving.

2. **A dangling external dependency.** `brainstorming` is referenced by `using-superpowers/SKILL.md:53,63`, `writing-plans/SKILL.md:16,23`, and `using-git-worktrees/SKILL.md:212` ("REQUIRED when design is approved"), but has never been bundled. It resolves only because the maintainer happens to have the upstream Superpowers plugin installed globally. A standalone ironmem install has a broken reference. `dispatching-parallel-agents` is dangling the same way (`references/codex-tools.md:25`, `references/gemini-tools.md:21`).

3. **No model routing.** The skills are model-agnostic, so every task in a plan runs at whatever tier the session started on. Meanwhile `commands/ultrareview-local.md:21-27` and `.codex-plugin/skills/using-superpowers/references/codex-tools.md:79-82` both already route by model and effort per role — the pattern exists in the repo but not in the workflow skills.

Additionally, ~600 of the 2,404 lines exist to compensate for weaker instruction-following (`using-superpowers`, injected at every SessionStart) or for harnesses without subagents (`executing-plans`), and are no longer earning their cost.

## Goals

- One authored source per skill; both harness copies generated, with drift caught by CI rather than review.
- Zero dependency on the upstream Superpowers plugin.
- Complexity-based routing across four tiers, with a shared policy and per-harness model lineups.
- Identical workflow behavior whether Claude or Codex is the implementer.
- Substantially less prose: ~1,030 authored lines replacing ~2,404 (~4,800 counting the duplicate).

## Non-goals

- Changing the `pr-review-toolkit` arrangement. Codex bundles it via `REQUIRED_CODEX_SKILLS`; Claude obtains it from a separate plugin. It is a review toolkit, not a workflow skill. Left as-is deliberately.
- Rewriting `docs/agent-guides/` (currently untracked) or the `/ultrareview-local` agent set.
- Migrating historical run artifacts under `.superpowers/sdd/`.

---

## Architecture

### Canonical source and generated output

```
skills/                                    # authored here, single source of truth
  vocab.toml                               # per-harness token values
  iron-spec/SKILL.md
  iron-plan/SKILL.md
  iron-build/SKILL.md
             references/tiers.md           # contains harness blocks
             prompts/implementer.md
             prompts/spec-reviewer.md
             prompts/quality-reviewer.md
  iron-tdd/SKILL.md
           references/testing-anti-patterns.md
        │
        │  scripts/sync_skills.py
        ▼
.claude-plugin/skills/        .codex-plugin/skills/      # generated, committed
```

Generated files are committed to git, consistent with the existing `scripts/sync_mcp_wrappers.py` pattern.

### Skill inventory

Four skills replace eight. All names carry an `iron-` prefix: the installer writes into `~/.claude/skills/` and `~/.codex/skills/`, which are flat namespaces shared with any globally-installed Superpowers, so unprefixed names would collide.

| New skill | Absorbs | Authored lines (target) |
|---|---|---|
| `iron-spec` | `brainstorming` (previously dangling) | ~120 |
| `iron-plan` | `writing-plans`, `plan-document-reviewer-prompt.md` | ~120 |
| `iron-build` | `subagent-driven-development`, `executing-plans`, `using-git-worktrees`, `finishing-a-development-branch`, `requesting-code-review` | ~180 + ~200 prompts |
| `iron-tdd` | `test-driven-development` | ~80 + ~300 reference |

Removed as separate skills. Four of these leave a small residue inside `iron-build` (noted in the Why column) rather than vanishing entirely — that residue is what "absorbs" means in the table above; the rest of their prose is deleted.

| Deleted | Lines | Why |
|---|---|---|
| `using-superpowers` | 117 | Compliance scaffolding ("even a 1% chance", rationalization table), injected at every SessionStart. Both harnesses discover skills natively from `description` frontmatter. |
| `references/{codex,copilot,gemini}-tools.md` | 200 | Tool-name mapping knowledge relocates into `vocab.toml`, where it is mechanically applied rather than interpreted at runtime. |
| `executing-plans` | 70 | Self-described degraded path for harnesses without subagents. Both harnesses have subagents. |
| `using-git-worktrees` | 218 | → ~25 lines in `iron-build`. Claude uses native `EnterWorktree` / `isolation: 'worktree'`; Codex uses git commands. Genuinely per-harness, so it lives in a harness block. |
| `finishing-a-development-branch` | 200 | → ~20 lines at the tail of `iron-build`. |
| `requesting-code-review` + `code-reviewer.md` | 251 | → `iron-build/prompts/quality-reviewer.md`. |

`iron-tdd` stays a separate skill rather than folding into `iron-build`, because "fix this bug with TDD" is a legitimate standalone invocation. Its SKILL.md compresses 371 → ~80 lines; `testing-anti-patterns.md` (299 lines) is retained nearly verbatim as real domain knowledge.

The Codex-only "Controller-Owned Loop" content is **unified upward** into canonical `iron-build`, so both harnesses receive it. This is a behavior change for Claude and is intended.

### Parity mechanism

Two substitution mechanisms, deliberately only two.

**1. Vocabulary tokens.** `{{DISPATCH}}`, `{{TODO}}`, `{{AGENT_DISPATCH}}` resolved from `vocab.toml`:

```toml
[claude]
DISPATCH = "Task tool"
TODO     = "TodoWrite"

[codex]
DISPATCH = "spawn_agent"
TODO     = "update_plan"
```

**2. Harness blocks.** For content that genuinely must differ:

```markdown
<!-- harness:codex -->
Create the worktree with `git worktree add ../<branch> -b <branch>`.
<!-- /harness -->
```

HTML comments, so the canonical file still reads as clean markdown.

Authoring discipline: prefer abstract verbs and keep harness blocks rare. A harness block is a visible, diffable admission that something differs — the opposite of the silent drift that produced the controller-loop defect.

**Generator rules.**

- Unknown `{{TOKEN}}` → hard error. A typo must never ship as literal text.
- Unclosed or unknown harness tag → hard error.
- Output byte-stable and deterministic, so the staleness check is meaningful.
- Every generated file gets a `<!-- GENERATED from skills/ — do not edit -->` header. This is the guardrail whose absence allowed the controller-loop drift.

**Enforcement.** `scripts/check_skills_sync.py` regenerates into a temp directory and diffs against committed output, exiting non-zero on mismatch. Wired into `.githooks/` and `.github/workflows`, mirroring `scripts/check_site_readme_sync.py`.

### Tier routing

Plan tasks carry a **tier**, never a model name. Tiers resolve to a `(model, effort)` pair — Claude's `output_config: {effort: …}` is the direct analog of Codex's `reasoning_effort`, which makes the two harnesses symmetric.

**Shared policy** (identical text in both copies, in `iron-plan/SKILL.md`):

| Tier | Task shape |
|---|---|
| `cheap` | Mechanical edits, renames/moves, boilerplate scaffolds, doc formatting, test fixtures |
| `standard` | Single-file feature with a tight spec, straightforward test writing, localized bugfix |
| `deep` | Cross-cutting changes, API and type design, non-trivial algorithmic work |
| `frontier` | Architecture and security escalation, concurrency and `unsafe` Rust, the hardest long-horizon work |

**Per-harness lineup** (`iron-build/references/tiers.md`, the only file that legitimately differs):

| Tier | Claude alias | Claude API id | Claude effort | Codex model | Codex effort |
|---|---|---|---|---|---|
| `cheap` | `haiku` | `claude-haiku-4-5` | *(unsupported)* | `gpt-5.3-spark` | `low` |
| `standard` | `sonnet` | `claude-sonnet-5` | `medium` | `gpt-5.6-luna` | `medium` |
| `deep` | `opus` | `claude-opus-5` | `xhigh` | `gpt-5.6-terra` | `high` |
| `frontier` | `fable` | `claude-fable-5` | `high` | `gpt-5.6-sol` | `high` |

The Claude alias is the operative value — subagent dispatch inside Claude Code takes `model: haiku|sonnet|opus|fable`, not full API ids. The API id column is for traceability only. Codex values are taken from `.codex-plugin/skills/using-superpowers/references/codex-tools.md:79-82`; `gpt-5.3-spark` is new and adds a cheap tier below `luna`.

`xhigh` on `deep` is the documented recommendation for coding and agentic work. **Haiku 4.5 does not accept `effort`** — passing it returns HTTP 400 — so the `cheap` row carries no effort value, and the generator must not synthesize one.

Two constraints on the Claude side that the implementation must respect:

- **Effort is not settable on every dispatch path.** Claude Code's `Agent` tool accepts `model` but has no `effort` parameter; only `Workflow`'s `agent()` accepts `effort`. So on the plain Agent-tool path the effort column is documentation, not an applied setting, and only the model alias takes effect. `iron-build` must therefore treat effort as best-effort: applied when dispatching through `Workflow`, recorded but inert otherwise. It must not claim an effort was applied when it wasn't — the ironmem drawer records the dispatch path alongside the tier so the routing dataset stays honest.
- **`frontier` can fail for reasons unrelated to the task.** Claude Fable 5 requires 30-day data retention and returns `400 invalid_request_error` on *every* request from an organization configured for zero data retention. Since ironmem ships to other users, `iron-build` must surface that 400 as a configuration problem and fall back to `deep` with an explicit message, rather than reporting it as a task failure.

**Two rules that make routing pay off**, both in the shared policy:

1. **Reviewer floor.** Reviewers run at least one tier above the implementer, and never below `standard`. A `cheap` implementation gets a `standard` spec-reviewer. Cheap implementer plus expensive reviewer is a better cost/quality point than the reverse, because review is what catches the cheap model's mistakes.

2. **Escalate-on-failure.** If a task's reviewer rejects twice, re-dispatch the implementer one tier higher, once. If that also fails, stop and ask the human. This makes under-routing self-correcting, so `iron-plan` can guess low with bounded risk.

---

## Data flow

```
idea → iron-spec  → docs/iron/specs/YYYY-MM-DD-<topic>-design.md
     → iron-plan  → docs/iron/plans/YYYY-MM-DD-<topic>.md   (tasks tagged with tier)
     → iron-build → per task: resolve tier → implementer → spec review → quality review
                                → commit → ironmem drawer
                  → run artifacts in .iron/runs/<date>-<topic>/
```

`docs/superpowers/` is renamed to `docs/iron/` via `git mv` so specs and plans stay in one place with history preserved (6 existing documents: 4 plans, 2 specs). Cross-references to the old path must be grepped and updated in the same commit.

`.superpowers/sdd/2026-07-29-mcp-response-compression/` is **not** migrated. It is real history from in-flight work; new runs go to `.iron/runs/` and the old directory is left orphaned rather than rewritten.

Per completed task, `iron-build` writes an ironmem drawer via `add_drawer` with a `logical_key`, recording `(task shape, tier used, review rounds needed, escalated y/n)`. Over time this is a dataset on which tier actually suffices for which work — the routing table improves from outcomes instead of staying a guess. This is the one capability neither a skill file nor a memory file provides.

## Error handling

| Condition | Behavior |
|---|---|
| Implementer returns `BLOCKED` / `NEEDS_CONTEXT` | Controller supplies context and re-dispatches, or stops for the human. Never silently marks the task complete. |
| Reviewer rejects | Return findings to the implementer. After two rejections, re-dispatch one tier higher (once). After that, stop and ask. |
| Unknown tier in a plan | Hard error at plan-parse time. No silent default to `standard` — a typo must not route work to the wrong model. |
| Task too large for `cheap` | `iron-plan` refuses to assign `cheap` when the task's file set won't fit Haiku 4.5's 200K context / 64K output. Over-routing is preferable to mid-implementation truncation. |
| Generated output stale | CI failure, not a warning. |
| Tier missing from a harness lineup | Installer validation failure (extends the existing `validate_packaged_skills()`). |
| `frontier` returns 400 under zero data retention | Report as a configuration problem, fall back to `deep`, state the substitution. Never surface as a task failure. |

## Testing

| Test | Covers |
|---|---|
| `scripts/test_sync_skills.py` | Vocabulary substitution, harness-block extraction, idempotency, and both hard-error paths (unknown `{{TOKEN}}`, unclosed harness tag). |
| `scripts/check_skills_sync.py` | Regenerate-and-diff; wired into `.githooks/` and CI. |
| Tier-parity test | Every tier named in `iron-plan`'s shared policy has a row in *every* harness `tiers.md`. This is the one file byte-comparison cannot check, so adding a fifth tier to the policy without both lineups becomes a test failure. |
| `scripts/test_install_ironmem.py` (extended) | New skill manifest, plus both branches of old-skill cleanup: removes when a base snapshot proves provenance, and **refuses** when no snapshot exists. |
| `scripts/check_collab_turn_templates.py` (updated) | Stage 2 — prompt templates after the `/collab` rework. |

## Migration

The installer writes into `~/.claude/skills/` and `~/.codex/skills/`. After upgrading, the eight *old* skill directories remain there and stay fully discoverable, so a model could invoke stale `writing-plans` instead of `iron-plan`. Renaming does not avoid this — it guarantees it.

The fix uses existing machinery. `install_skill_set()` writes three-way-merge base snapshots to `~/.claude/.ironmem-bases/skills/<name>` (`scripts/install-ironmem.sh:459-468`). That snapshot is proof of provenance:

- Base snapshot exists for `<old-name>` → ironmem installed it → remove the installed copy and the snapshot.
- No base snapshot → it is the user's own or upstream Superpowers' copy → **leave it** and print a warning.

Applied to all eight old names on both the Claude and Codex sides.

Manifest changes in `scripts/install-ironmem.sh:31-46`: `REQUIRED_SHARED_SKILLS` becomes the four `iron-*` names; `REQUIRED_CODEX_SKILLS` keeps `pr-review-toolkit`; `REQUIRED_CLAUDE_SKILLS` stays empty.

---

## Implementation stages

The work is too large for one plan. It splits at a natural boundary, because the skill layer is independently shippable.

**Stage 1 — the workflow layer.** Canonical `skills/`, `vocab.toml`, `sync_skills.py`, `check_skills_sync.py`, the four skills, tier tables, installer manifest and cleanup, `docs/superpowers` → `docs/iron` rename, tests. Ends with `/collab`'s two invocation points (`commands/collab.md:462`, `prompts/collab-turn-code-implement.md:66`) repointed at `iron-build` so everything still works. Independently testable and shippable.

**Stage 2 — the `/collab` trim.** Rework `commands/collab.md` (1,123 lines) and the 14 prompt templates (8 Claude, 6 Codex) onto the new layer, deleting workflow prose that now duplicates the skills. Update `check_collab_turn_templates.py`.

One spec covers both because Stage 2 cannot be specified without Stage 1's skill contract; the split is at the plan boundary.

## Consequences

- Authored surface drops from ~2,404 hand-maintained lines (~4,800 with the duplicate) to ~1,030, with both harness copies generated.
- Cross-harness drift becomes a CI failure. The controller-loop defect could not recur.
- ironmem no longer depends on the upstream Superpowers plugin for any skill.
- Claude gains the controller-owned loop it currently lacks.
- Every SessionStart stops paying for `using-superpowers`.
- Routing becomes real, and gets better over time from recorded outcomes.
- Cost: a build step and committed generated files; and the migration must delete stale installed skills without touching the user's own copies.
