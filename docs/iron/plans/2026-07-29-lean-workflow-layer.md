# Lean Workflow Layer — Stage 1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.
>
> Note the bootstrap irony: this plan *deletes* the skills named above. Use them from the currently-installed copies while executing; Task 10 is the last point at which they still exist. After this plan lands, the successor skills are `iron-plan` / `iron-build`.

**Goal:** Replace the eight vendored Superpowers skills with four generated, harness-parity `iron-*` skills driven by a single canonical `skills/` source, add tier-based model routing, and leave `/collab` working end-to-end.

**Architecture:** One authored tree at `skills/` plus `skills/vocab.toml`. `scripts/sync_skills.py` renders it into `.claude-plugin/skills/` and `.codex-plugin/skills/` using exactly two substitution mechanisms — `{{TOKEN}}` vocabulary lookup and `<!-- harness:NAME -->` blocks. Generated output is committed; `scripts/check_skills_sync.py` fails CI and the git hook on drift. The installer's skill manifest switches to the four `iron-*` names and removes superseded skills only where a base snapshot proves ironmem installed them.

**Tech Stack:** Python 3.11+ stdlib only (`tomllib`, `argparse`, `pathlib`, `unittest`), Bash, Markdown, GitHub Actions.

**Source documents:**
- `docs/iron/specs/2026-07-29-lean-workflow-layer-design.md` (approved design)
- `docs/iron/specs/2026-07-29-lean-workflow-layer-implementation-notes.md` (verified extension points, line numbers against `52b4569`)

**Stage 2 is out of scope.** The 1,123-line `commands/collab.md` trim and the 14 prompt templates get their own plan. Stage 1 touches those files only where a name would otherwise resolve to a skill that no longer exists.

---

## Global Constraints

Every task's requirements implicitly include this section.

1. **Claude Code's `Agent` tool has no `effort` parameter.** Only `Workflow`'s `agent()` accepts `effort`. On the plain Agent-tool path, a tier's effort value is documentation, not an applied setting — only the model alias takes effect. `iron-build` must never claim an effort was applied when it wasn't, and must record the dispatch path (`workflow` | `agent`) in its ironmem drawer alongside the tier.
2. **Haiku 4.5 rejects `effort` (HTTP 400).** The `cheap` tier carries a model and **no** effort value. The generator must not synthesize one; the tier-parity test must accept an absent effort on `cheap` and only on `cheap`.
3. **`scripts/install-ironmem.sh` is already in `COLLAB_EXACT_PATHS`** (`scripts/git_hook/manifest.py:35`), and `is_collab_protocol_path` is checked before any later-declared surface. Installer edits therefore classify `collab_protocol`, **not** `skills`. This plan accepts that deliberately (Task 7) and pins it with a test. Do not reorder `SURFACES` to steal the installer from the collab gate.
4. **Claude Fable 5 requires 30-day data retention.** An org configured for zero data retention gets `400 invalid_request_error` on every `frontier` request. `iron-build` must report that as a configuration problem, fall back to `deep`, state the substitution, and never surface it as a task failure.
5. **Python floor is 3.11** (`tomllib` is stdlib from 3.11). Scripts must fail with an explicit message, not an `ImportError` traceback, on older interpreters.
6. **Stdlib only** in `scripts/`. No new dependencies. Mirrors `scripts/sync_mcp_wrappers.py` and `scripts/check_site_readme_sync.py`.
7. **Exit-code convention:** `0` clean, `1` drift, `2` bad input / unresolvable ref. CI-visible failures print a GitHub annotation `::error title=<Title>::<detail>`.
8. **Generated output is byte-stable and committed**, LF line endings, one trailing newline, sorted traversal order. The staleness check is only meaningful if regeneration is deterministic.
9. **Zero dependency on the upstream Superpowers plugin.** After this plan, no shipped file may reference a skill ironmem does not itself install.
10. **Every generated file carries** `<!-- GENERATED from skills/ — do not edit -->`, placed after YAML frontmatter where frontmatter exists.

---

## File Structure

**Created — canonical source (authored):**

| Path | Responsibility |
|---|---|
| `skills/vocab.toml` | Per-harness token values. The only place harness tool names appear. |
| `skills/iron-spec/SKILL.md` | Requirements exploration → design doc. Absorbs `brainstorming`. |
| `skills/iron-plan/SKILL.md` | Spec → plan with tier-tagged tasks. Absorbs `writing-plans` + its reviewer prompt. Owns the **shared tier policy**. |
| `skills/iron-build/SKILL.md` | Controller-owned execution loop. Absorbs `subagent-driven-development`, `executing-plans`, `using-git-worktrees`, `finishing-a-development-branch`, `requesting-code-review`. |
| `skills/iron-build/references/tiers.md` | Per-harness `(model, effort)` lineup. The one file that legitimately differs between harnesses. |
| `skills/iron-build/prompts/implementer.md` | Implementer dispatch template. |
| `skills/iron-build/prompts/spec-reviewer.md` | Spec-compliance reviewer template. |
| `skills/iron-build/prompts/quality-reviewer.md` | Code-quality reviewer template. |
| `skills/iron-tdd/SKILL.md` | TDD discipline. Standalone-invocable. |
| `skills/iron-tdd/references/testing-anti-patterns.md` | Retained near-verbatim domain knowledge. |

**Created — tooling:**

| Path | Responsibility |
|---|---|
| `scripts/sync_skills.py` | Render canonical → both plugin roots. `--check` compares without writing. |
| `scripts/check_skills_sync.py` | CI/hook guard: regenerate into a temp dir, diff, annotate, exit 1 on drift. |
| `scripts/test_sync_skills.py` | Generator self-test (fixture-driven, no dependency on real skill content). |

**Modified:**

| Path | Change |
|---|---|
| `scripts/git_hook/manifest.py` | `SKILLS_EXACT_PATHS`, `is_skills_path`, `SURFACE_SKILLS`, `SURFACES` entry (before `SURFACE_DOCS`), `skills_sync_check` gate. |
| `scripts/test_run_git_hook.py` | Skills-surface classification tests; installer-collision pin; directory-walk coverage test. |
| `scripts/install-ironmem.sh` | `REQUIRED_SHARED_SKILLS` → four `iron-*`; new `LEGACY_SHARED_SKILLS` + `remove_legacy_skills()`. |
| `scripts/test_install_ironmem.py` | Both cleanup branches (snapshot present → removed; absent → kept + warning). |
| `scripts/check_collab_turn_templates.py` | Plan-path literal `docs/superpowers/` → `docs/iron/`. |
| `.github/workflows/ci.yml` | Two steps after `MCP wrapper drift check`. |
| `.gitignore` | Remove the `docs/superpowers/` line (see Task 9's decision note). |
| `.claude-plugin/skills/ATTRIBUTION.md`, `.codex-plugin/skills/ATTRIBUTION.md` | Derived-works list. |
| `.claude-plugin/commands/collab.md`, `.claude-plugin/prompts/collab-turn-code-implement.md`, `.codex-plugin/prompts/collab-batch-impl.md` | Repoint at `iron-build`. |
| `.claude-plugin/commands/evaluate-issue.md`, `.codex-plugin/prompts/evaluate-issue.md` | Repoint routing advice at `iron-plan` / `iron-build` / `iron-tdd`. |

**Deleted:** all eight skill directories under `.claude-plugin/skills/` and `.codex-plugin/skills/` (Codex keeps `pr-review-toolkit`).

---

## Two findings that change the brief

Read these before starting; both were verified against the working tree, and both contradict an assumption in the source documents.

**A. `docs/superpowers/` is gitignored, and only 2 of its 6 files are tracked.**

`.gitignore:17` is `docs/superpowers/`. `git ls-files docs/superpowers` returns exactly two paths:

```
docs/superpowers/plans/2026-07-26-review-diff-compression.md
docs/superpowers/specs/2026-07-26-review-diff-compression-design.md
```

The design doc's "renamed via `git mv` so history is preserved (6 existing documents)" is true for 2 of them. The other 4 are untracked working artifacts that `git mv` cannot move. Task 9 handles both sets and forces the `.gitignore` policy decision that the move creates. `docs/iron/` is **not** ignored — the two spec files are committed — so the directory is currently half-tracked, which is precisely the drift this whole spec exists to eliminate.

**B. There is a third live `/collab` invocation point, on the Codex side.**

The brief names `commands/collab.md:462` and `prompts/collab-turn-code-implement.md:66`. `.codex-plugin/prompts/collab-batch-impl.md:105,116` also invokes `subagent-driven-development` by name, and Codex is a supported `implementer`. Leaving it would break the Codex path the moment the installer stops shipping that skill. Task 10 repoints all three. It also repoints `evaluate-issue.md` (both harnesses), which *recommends* `writing-plans` — advisory, but it would recommend a skill that no longer exists.

---

## Task 1: Vocabulary file and generator core

**Files:**
- Create: `skills/vocab.toml`
- Create: `scripts/sync_skills.py`
- Test: `scripts/test_sync_skills.py`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `sync_skills.load_vocab(path: pathlib.Path) -> dict[str, dict[str, str]]`
  - `sync_skills.render(text: str, harness: str, vocab: dict[str, dict[str, str]], *, origin: str) -> str`
  - `sync_skills.SkillSyncError(Exception)` — raised for unknown token, unknown harness name, unclosed block. Carries a message naming `origin` and the offending value.
  - `sync_skills.GENERATED_HEADER = "<!-- GENERATED from skills/ — do not edit -->"`
  - `sync_skills.HARNESSES = ("claude", "codex")`

This task builds and tests `render` in isolation, against fixture strings. It does not walk the filesystem — Task 2 adds that. Splitting here is deliberate: `render` is where every hard-error rule lives, and it is worth its own test cycle.

- [ ] **Step 1: Write `skills/vocab.toml`**

```toml
# Canonical per-harness vocabulary for skills/. Every harness-specific tool
# name in an authored skill resolves from here via {{TOKEN}}; nothing else in
# skills/ may name a harness tool directly.
#
# Adding a token means adding it to EVERY harness table below. An unknown
# {{TOKEN}} is a hard error in scripts/sync_skills.py, so a token present for
# one harness and missing for another fails generation rather than shipping
# as literal text.

[claude]
DISPATCH       = "Task tool"
TODO           = "TodoWrite"
AGENT_DISPATCH = "Task(subagent_type=\"general-purpose\", model=<alias>, prompt=<full task text>)"
WORKTREE_NEW   = "EnterWorktree (native), or `git worktree add ../<branch> -b <branch>`"
SKILLS_HOME    = "~/.claude/skills"

[codex]
DISPATCH       = "spawn_agent"
TODO           = "update_plan"
AGENT_DISPATCH = "spawn_agent(agent_type=\"worker\", model=<model>, reasoning_effort=<effort>, message=<full task text>)"
WORKTREE_NEW   = "git worktree add ../<branch> -b <branch>"
SKILLS_HOME    = "~/.codex/skills"
```

- [ ] **Step 2: Write the failing tests**

Create `scripts/test_sync_skills.py`:

```python
#!/usr/bin/env python3
"""Self-test for scripts/sync_skills.py.

Stdlib-only unittest, run as `python3 scripts/test_sync_skills.py` (CI) or
under pytest. Fixture-driven: nothing here reads the real skills/ tree, so
the generator's contract is tested independently of skill content.
"""
from __future__ import annotations

import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import sync_skills  # noqa: E402

VOCAB = {
    "claude": {"DISPATCH": "Task tool", "TODO": "TodoWrite"},
    "codex": {"DISPATCH": "spawn_agent", "TODO": "update_plan"},
}


class RenderTests(unittest.TestCase):
    def render(self, text: str, harness: str) -> str:
        return sync_skills.render(text, harness, VOCAB, origin="fixture.md")

    def test_token_substitution_is_per_harness(self) -> None:
        text = "Dispatch with the {{DISPATCH}} and track via {{TODO}}.\n"
        self.assertEqual(
            self.render(text, "claude"),
            "Dispatch with the Task tool and track via TodoWrite.\n",
        )
        self.assertEqual(
            self.render(text, "codex"),
            "Dispatch with the spawn_agent and track via update_plan.\n",
        )

    def test_harness_block_kept_for_matching_harness(self) -> None:
        text = (
            "before\n"
            "<!-- harness:codex -->\n"
            "codex only\n"
            "<!-- /harness -->\n"
            "after\n"
        )
        self.assertEqual(self.render(text, "codex"), "before\ncodex only\nafter\n")

    def test_harness_block_dropped_for_other_harness(self) -> None:
        text = (
            "before\n"
            "<!-- harness:codex -->\n"
            "codex only\n"
            "<!-- /harness -->\n"
            "after\n"
        )
        self.assertEqual(self.render(text, "claude"), "before\nafter\n")

    def test_unknown_token_is_a_hard_error(self) -> None:
        with self.assertRaises(sync_skills.SkillSyncError) as ctx:
            self.render("use the {{DISPACH}}\n", "claude")
        self.assertIn("DISPACH", str(ctx.exception))
        self.assertIn("fixture.md", str(ctx.exception))

    def test_unclosed_harness_block_is_a_hard_error(self) -> None:
        text = "before\n<!-- harness:codex -->\nno terminator\n"
        with self.assertRaises(sync_skills.SkillSyncError) as ctx:
            self.render(text, "codex")
        self.assertIn("unclosed", str(ctx.exception).lower())

    def test_unknown_harness_name_in_block_is_a_hard_error(self) -> None:
        text = "<!-- harness:gemini -->\nx\n<!-- /harness -->\n"
        with self.assertRaises(sync_skills.SkillSyncError) as ctx:
            self.render(text, "claude")
        self.assertIn("gemini", str(ctx.exception))

    def test_stray_close_tag_is_a_hard_error(self) -> None:
        with self.assertRaises(sync_skills.SkillSyncError):
            self.render("before\n<!-- /harness -->\nafter\n", "claude")

    def test_token_inside_kept_block_is_substituted(self) -> None:
        text = "<!-- harness:codex -->\nrun {{DISPATCH}}\n<!-- /harness -->\n"
        self.assertEqual(self.render(text, "codex"), "run spawn_agent\n")

    def test_render_is_idempotent_on_token_free_text(self) -> None:
        text = "plain markdown, no tokens\n"
        once = self.render(text, "claude")
        self.assertEqual(self.render(once, "claude"), once)

    def test_nested_blocks_are_rejected(self) -> None:
        text = (
            "<!-- harness:codex -->\n"
            "<!-- harness:claude -->\n"
            "x\n"
            "<!-- /harness -->\n"
            "<!-- /harness -->\n"
        )
        with self.assertRaises(sync_skills.SkillSyncError):
            self.render(text, "codex")


if __name__ == "__main__":
    unittest.main()
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `python3 scripts/test_sync_skills.py`
Expected: FAIL — `ModuleNotFoundError: No module named 'sync_skills'`

- [ ] **Step 4: Write `scripts/sync_skills.py` (render layer only)**

```python
#!/usr/bin/env python3
"""Generate the per-harness skill trees under .claude-plugin/skills/ and
.codex-plugin/skills/ from the single authored source in skills/.

Two substitution mechanisms, deliberately only two:

  1. {{TOKEN}}                      -- resolved from skills/vocab.toml
  2. <!-- harness:NAME --> ... <!-- /harness -->

Every failure mode is a hard error, never a silent pass-through: a mistyped
token must not ship as literal text, and an unclosed block must not silently
swallow the rest of a file. Output is byte-stable so that
scripts/check_skills_sync.py's regenerate-and-diff is meaningful.
"""
from __future__ import annotations

import argparse
import pathlib
import re
import sys

if sys.version_info < (3, 11):  # tomllib is stdlib from 3.11
    sys.stderr.write(
        "ERROR: scripts/sync_skills.py requires Python 3.11+ (tomllib); "
        f"running under {sys.version.split()[0]}\n"
    )
    raise SystemExit(2)

import tomllib

ROOT = pathlib.Path(__file__).resolve().parents[1]
SOURCE = ROOT / "skills"
VOCAB_PATH = SOURCE / "vocab.toml"

HARNESSES = ("claude", "codex")
TARGETS = {
    "claude": ROOT / ".claude-plugin" / "skills",
    "codex": ROOT / ".codex-plugin" / "skills",
}

GENERATED_HEADER = "<!-- GENERATED from skills/ — do not edit -->"

# The generator owns only `iron-*` subtrees inside each target. ATTRIBUTION.md
# and .codex-plugin/skills/pr-review-toolkit/ are hand-maintained and must
# survive regeneration untouched.
OWNED_PREFIX = "iron-"

_TOKEN_RE = re.compile(r"\{\{([A-Z0-9_]+)\}\}")
_OPEN_RE = re.compile(r"^<!--\s*harness:([a-z0-9_-]+)\s*-->\s*$")
_CLOSE_RE = re.compile(r"^<!--\s*/harness\s*-->\s*$")


class SkillSyncError(Exception):
    """Raised for any condition that must fail generation rather than ship."""


def load_vocab(path: pathlib.Path = VOCAB_PATH) -> dict[str, dict[str, str]]:
    try:
        raw = tomllib.loads(path.read_text(encoding="utf-8"))
    except OSError as exc:
        raise SkillSyncError(f"cannot read vocab {path}: {exc}") from exc
    except tomllib.TOMLDecodeError as exc:
        raise SkillSyncError(f"malformed vocab {path}: {exc}") from exc

    missing = [h for h in HARNESSES if h not in raw]
    if missing:
        raise SkillSyncError(f"vocab {path} has no table(s) for harness {missing}")

    vocab: dict[str, dict[str, str]] = {}
    for harness in HARNESSES:
        table = raw[harness]
        if not isinstance(table, dict):
            raise SkillSyncError(f"vocab {path}: [{harness}] is not a table")
        vocab[harness] = {str(k): str(v) for k, v in table.items()}

    # A token defined for one harness and not another would render literally
    # for the second only when that line happens to be exercised. Catch the
    # asymmetry here instead.
    keysets = {h: frozenset(vocab[h]) for h in HARNESSES}
    reference = keysets[HARNESSES[0]]
    for harness, keys in keysets.items():
        if keys != reference:
            raise SkillSyncError(
                f"vocab {path}: harness tables disagree; "
                f"{harness} has {sorted(keys ^ reference)} that others do not"
            )
    return vocab


def _strip_harness_blocks(text: str, harness: str, vocab: dict[str, dict[str, str]], origin: str) -> str:
    out: list[str] = []
    open_harness: str | None = None
    open_line = 0

    for lineno, line in enumerate(text.split("\n"), start=1):
        opened = _OPEN_RE.match(line)
        if opened:
            if open_harness is not None:
                raise SkillSyncError(
                    f"{origin}:{lineno}: nested harness block "
                    f"(harness:{open_harness} opened at line {open_line} is still open)"
                )
            name = opened.group(1)
            if name not in vocab:
                raise SkillSyncError(
                    f"{origin}:{lineno}: unknown harness {name!r}; "
                    f"declared harnesses are {sorted(vocab)}"
                )
            open_harness = name
            open_line = lineno
            continue

        if _CLOSE_RE.match(line):
            if open_harness is None:
                raise SkillSyncError(f"{origin}:{lineno}: stray <!-- /harness --> with no open block")
            open_harness = None
            continue

        if open_harness is None or open_harness == harness:
            out.append(line)

    if open_harness is not None:
        raise SkillSyncError(
            f"{origin}: unclosed harness block <!-- harness:{open_harness} --> opened at line {open_line}"
        )
    return "\n".join(out)


def _substitute_tokens(text: str, harness: str, vocab: dict[str, dict[str, str]], origin: str) -> str:
    table = vocab[harness]

    def replace(match: re.Match[str]) -> str:
        token = match.group(1)
        if token not in table:
            raise SkillSyncError(
                f"{origin}: unknown token {{{{{token}}}}} for harness {harness!r}; "
                f"declared tokens are {sorted(table)}"
            )
        return table[token]

    return _TOKEN_RE.sub(replace, text)


def render(text: str, harness: str, vocab: dict[str, dict[str, str]], *, origin: str) -> str:
    """Render authored `text` for `harness`. Blocks first, then tokens: every
    block is rendered by exactly one harness, so a token inside a block is
    still validated -- just on that harness's pass.
    """
    if harness not in vocab:
        raise SkillSyncError(f"{origin}: unknown harness {harness!r}")
    stripped = _strip_harness_blocks(text, harness, vocab, origin)
    return _substitute_tokens(stripped, harness, vocab, origin)
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `python3 scripts/test_sync_skills.py -v`
Expected: PASS, 10 tests.

- [ ] **Step 6: Commit**

```bash
git add skills/vocab.toml scripts/sync_skills.py scripts/test_sync_skills.py
git commit -m "feat(skills): canonical vocab + generator render layer

Two substitution mechanisms only: {{TOKEN}} from skills/vocab.toml and
<!-- harness:NAME --> blocks. Every failure mode -- unknown token, unknown
harness, unclosed/nested/stray block, asymmetric vocab tables -- is a hard
error, so a typo cannot ship as literal text."
```

---

## Task 2: Generator file walk, header injection, and `--check`

**Files:**
- Modify: `scripts/sync_skills.py` (append to the module from Task 1)
- Modify: `scripts/test_sync_skills.py` (append a second test class)

**Interfaces:**
- Consumes: `render`, `load_vocab`, `GENERATED_HEADER`, `TARGETS`, `OWNED_PREFIX` from Task 1.
- Produces:
  - `sync_skills.inject_header(text: str) -> str` — inserts `GENERATED_HEADER` after YAML frontmatter, or at the top when there is none.
  - `sync_skills.plan(source: pathlib.Path = SOURCE) -> dict[str, dict[str, str]]` — returns `{harness: {relative_posix_path: rendered_text}}`. Pure; no writes.
  - `sync_skills.write(rendered: dict[str, dict[str, str]], targets: dict[str, pathlib.Path]) -> list[str]` — writes and prunes; returns changed relative paths.
  - `main()` with `--check`.

- [ ] **Step 1: Write the failing tests**

Append to `scripts/test_sync_skills.py`, above the `if __name__` block:

```python
import shutil
import tempfile


class HeaderTests(unittest.TestCase):
    def test_header_goes_after_yaml_frontmatter(self) -> None:
        text = "---\nname: iron-build\ndescription: x\n---\n\n# Iron Build\n"
        result = sync_skills.inject_header(text)
        lines = result.split("\n")
        self.assertEqual(lines[0], "---")
        self.assertEqual(lines[3], "---")
        self.assertEqual(lines[4], sync_skills.GENERATED_HEADER)
        self.assertIn("# Iron Build", result)

    def test_header_goes_on_top_without_frontmatter(self) -> None:
        result = sync_skills.inject_header("# Tiers\n")
        self.assertTrue(result.startswith(sync_skills.GENERATED_HEADER + "\n"))

    def test_header_is_not_duplicated(self) -> None:
        once = sync_skills.inject_header("# Tiers\n")
        self.assertEqual(sync_skills.inject_header(once), once)


class WalkTests(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = pathlib.Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, self.tmp)
        self.source = self.tmp / "skills"
        (self.source / "iron-demo").mkdir(parents=True)
        (self.source / "vocab.toml").write_text(
            '[claude]\nDISPATCH = "Task tool"\n\n[codex]\nDISPATCH = "spawn_agent"\n',
            encoding="utf-8",
        )
        (self.source / "iron-demo" / "SKILL.md").write_text(
            "---\nname: iron-demo\ndescription: demo\n---\n\nUse the {{DISPATCH}}.\n",
            encoding="utf-8",
        )
        self.targets = {
            "claude": self.tmp / "claude" / "skills",
            "codex": self.tmp / "codex" / "skills",
        }

    def test_plan_renders_every_harness(self) -> None:
        rendered = sync_skills.plan(self.source)
        self.assertEqual(sorted(rendered), ["claude", "codex"])
        self.assertIn("Use the Task tool.", rendered["claude"]["iron-demo/SKILL.md"])
        self.assertIn("Use the spawn_agent.", rendered["codex"]["iron-demo/SKILL.md"])

    def test_vocab_is_never_emitted(self) -> None:
        rendered = sync_skills.plan(self.source)
        for files in rendered.values():
            self.assertNotIn("vocab.toml", files)

    def test_write_is_idempotent(self) -> None:
        rendered = sync_skills.plan(self.source)
        first = sync_skills.write(rendered, self.targets)
        self.assertTrue(first)
        second = sync_skills.write(rendered, self.targets)
        self.assertEqual(second, [])

    def test_write_prunes_stale_iron_dirs_only(self) -> None:
        claude = self.targets["claude"]
        (claude / "iron-gone").mkdir(parents=True)
        (claude / "iron-gone" / "SKILL.md").write_text("stale\n", encoding="utf-8")
        (claude / "pr-review-toolkit").mkdir(parents=True)
        (claude / "pr-review-toolkit" / "SKILL.md").write_text("keep\n", encoding="utf-8")
        (claude / "ATTRIBUTION.md").write_text("keep\n", encoding="utf-8")

        sync_skills.write(sync_skills.plan(self.source), self.targets)

        self.assertFalse((claude / "iron-gone").exists())
        self.assertTrue((claude / "pr-review-toolkit" / "SKILL.md").is_file())
        self.assertTrue((claude / "ATTRIBUTION.md").is_file())

    def test_generated_output_carries_header(self) -> None:
        rendered = sync_skills.plan(self.source)
        self.assertIn(sync_skills.GENERATED_HEADER, rendered["claude"]["iron-demo/SKILL.md"])
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `python3 scripts/test_sync_skills.py -v`
Expected: FAIL — `AttributeError: module 'sync_skills' has no attribute 'inject_header'`

- [ ] **Step 3: Append the walk layer to `scripts/sync_skills.py`**

```python
def inject_header(text: str) -> str:
    """Place GENERATED_HEADER after YAML frontmatter, or at the top.

    Frontmatter must remain the very first bytes of a SKILL.md -- both
    harnesses parse `name`/`description` from it for discovery -- so the
    header cannot simply be prepended.
    """
    if GENERATED_HEADER in text:
        return text
    lines = text.split("\n")
    if lines and lines[0] == "---":
        for index in range(1, len(lines)):
            if lines[index] == "---":
                lines.insert(index + 1, GENERATED_HEADER)
                return "\n".join(lines)
        raise SkillSyncError("frontmatter opened with '---' but never closed")
    return GENERATED_HEADER + "\n" + text


def plan(source: pathlib.Path = SOURCE) -> dict[str, dict[str, str]]:
    """Render every authored file for every harness. Pure: no writes."""
    vocab = load_vocab(source / "vocab.toml")
    files = sorted(
        path for path in source.rglob("*") if path.is_file() and path.name != "vocab.toml"
    )
    rendered: dict[str, dict[str, str]] = {harness: {} for harness in HARNESSES}
    for path in files:
        relative = path.relative_to(source).as_posix()
        if not relative.startswith(OWNED_PREFIX):
            raise SkillSyncError(
                f"{relative}: every authored skill directory must be named "
                f"{OWNED_PREFIX}* (the generator only owns that prefix in the targets)"
            )
        text = path.read_text(encoding="utf-8")
        for harness in HARNESSES:
            body = render(text, harness, vocab, origin=relative)
            if not body.endswith("\n"):
                body += "\n"
            rendered[harness][relative] = inject_header(body)
    return rendered


def write(
    rendered: dict[str, dict[str, str]], targets: dict[str, pathlib.Path]
) -> list[str]:
    """Write rendered output and prune stale generated files.

    Only `iron-*` subtrees are owned. ATTRIBUTION.md and Codex's
    pr-review-toolkit are hand-maintained and survive untouched.
    """
    changed: list[str] = []
    for harness, files in rendered.items():
        root = targets[harness]
        for relative, body in sorted(files.items()):
            destination = root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            if not destination.exists() or destination.read_text(encoding="utf-8") != body:
                destination.write_text(body, encoding="utf-8", newline="\n")
                changed.append(f"{root.name}/{relative}")

        expected = set(files)
        if root.is_dir():
            for path in sorted(root.rglob("*")):
                if not path.is_file():
                    continue
                relative = path.relative_to(root).as_posix()
                if not relative.startswith(OWNED_PREFIX):
                    continue
                if relative not in expected:
                    path.unlink()
                    changed.append(f"{root.name}/{relative} (removed)")
            for path in sorted(root.rglob("*"), reverse=True):
                if path.is_dir() and path.name.startswith(OWNED_PREFIX) and not any(path.iterdir()):
                    path.rmdir()
    return changed


def diff(rendered: dict[str, dict[str, str]], targets: dict[str, pathlib.Path]) -> list[str]:
    """Relative paths whose committed content differs from `rendered`."""
    drifted: list[str] = []
    for harness, files in rendered.items():
        root = targets[harness]
        for relative, body in sorted(files.items()):
            destination = root / relative
            if not destination.is_file():
                drifted.append(f"{root.name}/{relative} (missing)")
            elif destination.read_text(encoding="utf-8") != body:
                drifted.append(f"{root.name}/{relative} (stale)")
        if root.is_dir():
            expected = set(files)
            for path in sorted(root.rglob("*")):
                if path.is_file():
                    relative = path.relative_to(root).as_posix()
                    if relative.startswith(OWNED_PREFIX) and relative not in expected:
                        drifted.append(f"{root.name}/{relative} (orphaned)")
    return drifted


def main() -> int:
    parser = argparse.ArgumentParser(description="generate per-harness skills from skills/")
    parser.add_argument("--check", action="store_true", help="fail if generated output is stale")
    args = parser.parse_args()

    try:
        rendered = plan()
    except SkillSyncError as exc:
        print(f"sync_skills: {exc}", file=sys.stderr)
        return 2

    if args.check:
        drifted = diff(rendered, TARGETS)
        if drifted:
            for entry in drifted:
                print(f"skills drifted: {entry}", file=sys.stderr)
            print("Run: python3 scripts/sync_skills.py", file=sys.stderr)
            return 1
        print("generated skills match skills/")
        return 0

    changed = write(rendered, TARGETS)
    for entry in changed:
        print(f"wrote {entry}")
    if not changed:
        print("generated skills already up to date")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `python3 scripts/test_sync_skills.py -v`
Expected: PASS, 18 tests.

- [ ] **Step 5: Commit**

```bash
git add scripts/sync_skills.py scripts/test_sync_skills.py
git commit -m "feat(skills): generator file walk, header injection, --check

Owns only iron-* subtrees in each plugin root, so ATTRIBUTION.md and Codex's
pr-review-toolkit survive regeneration. Stale iron-* files are pruned, which
is what makes deleting a canonical file actually delete both copies."
```

---

## Task 3: Author `iron-spec` and `iron-plan`

**Files:**
- Create: `skills/iron-spec/SKILL.md` (~120 lines)
- Create: `skills/iron-plan/SKILL.md` (~120 lines)

**Interfaces:**
- Consumes: `{{TODO}}` and `{{AGENT_DISPATCH}}` from `skills/vocab.toml`.
- Produces: the **shared tier policy** table and the two routing rules, which `iron-build/references/tiers.md` (Task 5) resolves and the tier-parity test (Task 6) reads. The tier names are exactly `cheap`, `standard`, `deep`, `frontier` — lowercase, no others.
- Produces: the plan-file path convention `docs/iron/plans/YYYY-MM-DD-<topic>.md` and the spec path `docs/iron/specs/YYYY-MM-DD-<topic>-design.md`, both of which Task 9 and Task 10 depend on.

- [ ] **Step 1: Write `skills/iron-spec/SKILL.md`**

Frontmatter verbatim (both harnesses discover skills from these two fields, so they are load-bearing, not decoration):

```markdown
---
name: iron-spec
description: Use before any creative work - creating features, building components, adding functionality, or modifying behavior. Explores intent, requirements, and design, and writes an approved design doc before implementation.
---
```

Required content, in order. Source material to compress is the upstream `brainstorming` skill's contract as it is *used* by `writing-plans/SKILL.md:16,23` and `using-git-worktrees/SKILL.md:212` — those call sites are the only surviving evidence of the never-bundled skill, so this is authored fresh against them rather than ported.

| Section | Must contain |
|---|---|
| `# Iron Spec` + one-line purpose | "Turn an idea into an approved design document before any code is written." |
| `## When to Use` | Triggered by "let's build X", "add feature Y", "change how Z behaves". Not for bug fixes with a known cause — that is `iron-tdd`. |
| `## The Loop` | Ask one question at a time. Never batch questions. Prefer concrete alternatives over open-ended prompts. Restate the problem in your own words and get agreement before proposing solutions. |
| `## What to Nail Down` | Problem statement; goals; explicit non-goals; architecture sketch; data flow; error handling table; testing strategy; consequences. These are the section headings the output doc must carry — they match `docs/iron/specs/2026-07-29-lean-workflow-layer-design.md`, which is the worked example. |
| `## Output` | Write to `docs/iron/specs/YYYY-MM-DD-<topic>-design.md` with a `**Status:**` line. Status is `Draft` until the human says otherwise, then `Approved design, pending implementation plan`. |
| `## Handoff` | When status reaches approved, the next step is `iron-plan`. Do not start implementing from a spec. |
| `## Red Flags` | Proposing a solution before the problem is agreed; batching questions; writing code during specification; marking a spec approved without the human saying so; a spec with no non-goals section. |

Hard requirement: the file must contain no reference to `brainstorming`, `superpowers`, or any skill ironmem does not install (Global Constraint 9).

- [ ] **Step 2: Write `skills/iron-plan/SKILL.md`**

Frontmatter verbatim:

```markdown
---
name: iron-plan
description: Use when you have a spec or requirements for a multi-step task, before touching code. Produces a tier-tagged implementation plan.
---
```

Compress `.claude-plugin/skills/writing-plans/SKILL.md` (152 lines) plus `writing-plans/plan-document-reviewer-prompt.md` (49 lines) into ~120. Retain from the source, near-verbatim: the plan-document header block, the "File Structure" guidance, "Task Right-Sizing", "Bite-Sized Task Granularity", "No Placeholders" (all six bullets), and "Self-Review" (all three checks). Drop the `dot`-graph decision diagrams and the "Execution Handoff" menu — `iron-build` is now the only execution path.

Three changes from the source that are not compression:

1. **Plan location** is `docs/iron/plans/YYYY-MM-DD-<feature-name>.md`, not `docs/superpowers/plans/`.
2. **Handoff** names `iron-build` as the single execution path.
3. **New section `## Tier Routing`** — this is the shared policy, and its text must be identical in both generated copies (no harness block, no tokens). Verbatim:

```markdown
## Tier Routing

Every task carries a **tier**, never a model name. Write it as a `**Tier:**`
line directly under the task heading. Tiers resolve to a concrete
`(model, effort)` pair in `iron-build/references/tiers.md`, which is the only
file that differs per harness.

| Tier | Task shape |
|---|---|
| `cheap` | Mechanical edits, renames/moves, boilerplate scaffolds, doc formatting, test fixtures |
| `standard` | Single-file feature with a tight spec, straightforward test writing, localized bugfix |
| `deep` | Cross-cutting changes, API and type design, non-trivial algorithmic work |
| `frontier` | Architecture and security escalation, concurrency and `unsafe` Rust, the hardest long-horizon work |

Two rules make routing pay off:

1. **Reviewer floor.** Reviewers run at least one tier above the implementer,
   and never below `standard`. A `cheap` implementation gets a `standard`
   spec-reviewer. Cheap implementer plus expensive reviewer is a better
   cost/quality point than the reverse, because review is what catches the
   cheap model's mistakes.

2. **Escalate on failure.** If a task's reviewer rejects twice, the controller
   re-dispatches the implementer one tier higher, once. If that also fails, it
   stops and asks the human. Under-routing is therefore self-correcting, so
   guess low.

**Never assign `cheap` to a task whose file set will not fit Haiku 4.5's 200K
context / 64K output.** Over-routing costs money; mid-implementation truncation
costs the task. Count the files the task must read, not just the files it
writes.

An unrecognized tier string is a hard error when `iron-build` parses the plan.
There is no default. A typo must not route work to the wrong model.
```

- [ ] **Step 3: Verify both files render for both harnesses**

Run: `python3 scripts/sync_skills.py --check`
Expected: exit 1 with `skills drifted: skills/iron-spec/SKILL.md (missing)` and siblings — generation has not been run yet, which is the correct state at this step. It confirms `plan()` parses both files without raising `SkillSyncError` (a parse error would exit 2 with a `sync_skills:` message instead).

- [ ] **Step 4: Confirm the tier vocabulary is exactly four lowercase names**

Run: `grep -oE '\`(cheap|standard|deep|frontier)\`' skills/iron-plan/SKILL.md | sort -u`
Expected: exactly four lines — `` `cheap` ``, `` `deep` ``, `` `frontier` ``, `` `standard` ``.

- [ ] **Step 5: Commit**

```bash
git add skills/iron-spec skills/iron-plan
git commit -m "feat(skills): author iron-spec and iron-plan

iron-spec absorbs the never-bundled brainstorming skill, closing the dangling
upstream dependency. iron-plan absorbs writing-plans plus its reviewer prompt
and owns the shared four-tier routing policy, including the reviewer floor and
escalate-on-failure rules."
```

---

## Task 4: Author `iron-tdd`

**Files:**
- Create: `skills/iron-tdd/SKILL.md` (~80 lines)
- Create: `skills/iron-tdd/references/testing-anti-patterns.md` (299 lines, near-verbatim)

**Interfaces:**
- Consumes: nothing from earlier tasks except the generator.
- Produces: `iron-tdd` as a standalone-invocable skill name, referenced by `iron-build`'s implementer prompt (Task 5) and by `evaluate-issue.md` (Task 10).

`iron-tdd` stays separate rather than folding into `iron-build` because "fix this bug with TDD" is a legitimate standalone invocation.

- [ ] **Step 1: Copy the anti-patterns reference near-verbatim**

```bash
mkdir -p skills/iron-tdd/references
cp .claude-plugin/skills/test-driven-development/testing-anti-patterns.md \
   skills/iron-tdd/references/testing-anti-patterns.md
```

Then edit only what breaks: replace any reference to `test-driven-development` with `iron-tdd`, and any `docs/superpowers/` path with `docs/iron/`. This file is real domain knowledge and is retained deliberately — do not compress it.

Run: `grep -n "superpowers\|test-driven-development\|writing-plans\|subagent-driven" skills/iron-tdd/references/testing-anti-patterns.md`
Expected: no output.

- [ ] **Step 2: Write `skills/iron-tdd/SKILL.md`**

Frontmatter verbatim:

```markdown
---
name: iron-tdd
description: Use when implementing any feature or bugfix, before writing implementation code. Red-green-refactor discipline with a catalogue of testing anti-patterns.
---
```

Compress `.claude-plugin/skills/test-driven-development/SKILL.md` (371 lines) to ~80. Retain: the red-green-refactor cycle as numbered steps; the rule that a test must be *run and observed failing* before implementation; the "minimal implementation" rule; the guidance that a test asserting on mocks is not a test; and a pointer to `./references/testing-anti-patterns.md`. Drop the `dot` graphs and the extended worked examples.

One required addition, because the harnesses differ in how a test run is invoked and the source assumed a shell:

```markdown
## The Cycle

1. Write one failing test that states the behavior you want.
2. **Run it and read the failure.** A test you have not watched fail is not a
   test — it is an assertion you hope is wired up. Record the exact failure
   message.
3. Write the minimal implementation that makes it pass. Not the general
   solution; the minimal one.
4. Run the test again and confirm it passes.
5. Refactor with the test green. Re-run after every refactor.
6. Commit.

Never write implementation code before step 2 has produced an observed failure.
If you cannot make the test fail, the test is wrong.
```

- [ ] **Step 3: Verify it parses**

Run: `python3 scripts/sync_skills.py --check`
Expected: exit 1 listing missing `iron-tdd/...` entries alongside the Task 3 files — not exit 2.

- [ ] **Step 4: Commit**

```bash
git add skills/iron-tdd
git commit -m "feat(skills): author iron-tdd

SKILL.md compressed 371 -> ~80 lines; testing-anti-patterns.md retained
near-verbatim as real domain knowledge. Stays a separate skill because
'fix this bug with TDD' is a legitimate standalone invocation."
```

---

## Task 5: Author `iron-build`, its tier lineup, and its three prompts

**Files:**
- Create: `skills/iron-build/SKILL.md` (~180 lines)
- Create: `skills/iron-build/references/tiers.md`
- Create: `skills/iron-build/prompts/implementer.md`
- Create: `skills/iron-build/prompts/spec-reviewer.md`
- Create: `skills/iron-build/prompts/quality-reviewer.md`

**Interfaces:**
- Consumes: the tier names and two routing rules from `iron-plan` (Task 3); `iron-tdd` by name (Task 4); `{{DISPATCH}}`, `{{TODO}}`, `{{AGENT_DISPATCH}}`, `{{WORKTREE_NEW}}` from `skills/vocab.toml`.
- Produces, and these are contracts other tasks depend on:
  - A section heading spelled exactly `## Finishing the Branch`. Task 10 rewrites `/collab`'s stop-boundary language to name it, replacing the current references to the `finishing-a-development-branch` skill.
  - A documented halt contract: a controller may instruct `iron-build` to stop before that section.
  - The four implementer statuses `DONE`, `DONE_WITH_CONCERNS`, `BLOCKED`, `NEEDS_CONTEXT`, unchanged from the source so `/collab`'s existing failure handling keeps working.
  - The ironmem drawer schema recorded per completed task.

- [ ] **Step 1: Write `skills/iron-build/references/tiers.md`**

This is the one file that legitimately differs per harness, so it is almost entirely harness blocks. Verbatim:

```markdown
# Tier Lineup

`iron-plan` assigns a tier. This file resolves it to a concrete model. The tier
names are fixed: `cheap`, `standard`, `deep`, `frontier`. An unrecognized tier
is a hard error at plan-parse time — never default to `standard`.

<!-- harness:claude -->
| Tier | Model alias | API id | Effort |
|---|---|---|---|
| `cheap` | `haiku` | `claude-haiku-4-5` | *(unsupported — do not pass)* |
| `standard` | `sonnet` | `claude-sonnet-5` | `medium` |
| `deep` | `opus` | `claude-opus-5` | `xhigh` |
| `frontier` | `fable` | `claude-fable-5` | `high` |

The **model alias** is the operative value. Subagent dispatch takes
`model: haiku|sonnet|opus|fable`, not a full API id; the API id column is for
traceability only.

## Effort is not settable on every dispatch path

The `Agent` tool accepts `model` but has **no `effort` parameter**. Only
`Workflow`'s `agent()` accepts `effort`. So:

- Dispatching through `Workflow` → the effort column is applied.
- Dispatching through the plain `Agent` tool → **only the model takes effect**.
  The effort column is documentation.

Record which path you used. Never report that an effort was applied when it
was not — the routing dataset is only worth keeping if it is honest.

**Haiku 4.5 rejects `effort` outright** (HTTP 400). The `cheap` row carries no
effort value on purpose. Do not synthesize one, on either path.

## When `frontier` returns 400

Claude Fable 5 requires 30-day data retention. An organization configured for
zero data retention gets `400 invalid_request_error` on *every* request,
regardless of the task. ironmem ships to other users, so treat that 400 as a
**configuration problem, not a task failure**:

1. Say plainly that `frontier` is unavailable under this org's data-retention
   setting.
2. Fall back to `deep` and state the substitution.
3. Continue. Do not report the task as failed, and do not retry `frontier`
   again in this run.
<!-- /harness -->

<!-- harness:codex -->
| Tier | Model | Reasoning effort |
|---|---|---|
| `cheap` | `gpt-5.3-spark` | `low` |
| `standard` | `gpt-5.6-luna` | `medium` |
| `deep` | `gpt-5.6-terra` | `high` |
| `frontier` | `gpt-5.6-sol` | `high` |

Both values are settable on every dispatch — `reasoning_effort` is a direct
parameter of `spawn_agent`, so unlike the Claude lineup there is no
best-effort caveat here. Record the values you passed.
<!-- /harness -->
```

- [ ] **Step 2: Write the three prompt templates**

`skills/iron-build/prompts/implementer.md` — port `.claude-plugin/skills/subagent-driven-development/implementer-prompt.md` (113 lines) with these changes:
- The dispatch line becomes `{{AGENT_DISPATCH}}` instead of the hard-coded `Task tool (general-purpose):`.
- "Write tests (following TDD if task says to)" becomes "Write tests using the `iron-tdd` discipline: one failing test, observed failing, then the minimal implementation."
- Everything else — "Before You Begin", "Code Organization", "When You're in Over Your Head", "Before Reporting Back: Self-Review", "Report Format" with its four statuses — is retained verbatim. It is the most load-bearing prose in the whole set.

`skills/iron-build/prompts/spec-reviewer.md` — port `spec-reviewer-prompt.md` (61 lines) verbatim except the dispatch line, which becomes `{{AGENT_DISPATCH}}`. The "CRITICAL: Do Not Trust the Report" section is retained word for word.

`skills/iron-build/prompts/quality-reviewer.md` — merge `code-quality-reviewer-prompt.md` (26 lines) with the substantive checklist from `requesting-code-review/code-reviewer.md` (146 lines), targeting ~90 lines. The merged file must:
- Use `{{AGENT_DISPATCH}}` for dispatch.
- Not reference the `code-reviewer` named agent (a Claude-plugin agent, not a skill ironmem's skill layer can assume) — inline the review criteria instead.
- Keep the four extra checks from `code-quality-reviewer-prompt.md:20-24` (single responsibility, independent testability, plan file structure, new-file size — with its "don't flag pre-existing sizes" caveat).
- Keep the `Strengths / Issues (Critical|Important|Minor) / Assessment` return format, which `/collab` already parses.
- Take `BASE_SHA` / `HEAD_SHA` inputs, as the current template does.

- [ ] **Step 3: Write `skills/iron-build/SKILL.md`**

Frontmatter verbatim:

```markdown
---
name: iron-build
description: Use when executing an implementation plan - dispatches a fresh subagent per task at the task's routed tier, with spec-compliance then code-quality review after each, and runs the whole plan without handing control back.
---
```

Required sections, in order:

| Section | Must contain |
|---|---|
| `# Iron Build` | One-line purpose: fresh subagent per task, two-stage review, controller-owned loop. |
| `## Controller-Owned Loop` | **Unified upward from the Codex-only copy** (`.codex-plugin/skills/subagent-driven-development/SKILL.md:17-34`). This is a deliberate behavior change for Claude. Verbatim below. |
| `## Workspace` | ~25 lines replacing `using-git-worktrees` (218). Uses `{{WORKTREE_NEW}}`. Rule: never start implementation on `main`/`master` without explicit consent. |
| `## Per-Task Cycle` | Resolve tier → dispatch implementer → handle status → spec review → quality review → commit → drawer → mark complete in `{{TODO}}` → next task. No `dot` graphs. |
| `## Resolving a Tier` | Read `**Tier:**` from the task. Look it up in `./references/tiers.md`. Unknown tier → stop, name the bad value, ask. Apply the reviewer floor (≥ one tier above the implementer, never below `standard`). |
| `## Handling Implementer Status` | The four statuses, ported from `subagent-driven-development/SKILL.md:102-118` with the escalate-on-failure rule folded in. Verbatim below. |
| `## Recording the Outcome` | The drawer schema, verbatim below. |
| `## Finishing the Branch` | ~20 lines replacing `finishing-a-development-branch` (200): present merge / PR / cleanup options, never pick for the human. **Plus the halt contract, verbatim below.** |
| `## Red Flags` | Ported from `subagent-driven-development/SKILL.md:234-263`, trimmed. |

`## Controller-Owned Loop`, verbatim:

```markdown
## Controller-Owned Loop

Once this skill starts, the controller owns the whole execution loop. Keep
going until one of these happens:

1. Every plan task is implemented, reviewed, committed, and marked complete,
   and the *Finishing the Branch* step has run
2. An implementer or reviewer reports `BLOCKED` or `NEEDS_CONTEXT`
3. The human explicitly interrupts or redirects the work

**Do not hand control back just because one task completed.** A task is
complete only when:

- implementation is done
- spec compliance review is approved
- code quality review is approved
- the task-scoped work is committed
- the task is marked complete in {{TODO}}

Then immediately dispatch the next task.
```

`## Handling Implementer Status`, verbatim:

```markdown
## Handling Implementer Status

**DONE** — proceed to spec compliance review.

**DONE_WITH_CONCERNS** — read the concerns before proceeding. If they touch
correctness or scope, address them before review. If they are observations
("this file is getting large"), note them and proceed.

**NEEDS_CONTEXT** — supply the missing context and re-dispatch at the same
tier. A context gap is not a capability gap.

**BLOCKED** — assess the blocker:

1. Context problem → provide more context, re-dispatch at the same tier
2. Reasoning problem → re-dispatch one tier higher
3. Task too large → split it
4. Plan is wrong → stop and escalate to the human

**Escalate on failure.** If the reviewer rejects the same task twice,
re-dispatch the implementer one tier higher — once. If that also fails, stop
and ask the human. Never re-dispatch at the same tier with the same context
and expect a different result.

**Never** silently mark a task complete after a `BLOCKED` or `NEEDS_CONTEXT`.
```

`## Recording the Outcome`, verbatim:

```markdown
## Recording the Outcome

After each completed task, write one ironmem drawer with a `logical_key` so
the latest state overwrites the previous copy rather than accumulating:

    add_drawer(
      logical_key = "iron-build:<plan-slug>:task-<n>",
      content = {
        "task_shape":    "<one line: what kind of work this was>",
        "tier_assigned": "cheap|standard|deep|frontier",
        "tier_used":     "cheap|standard|deep|frontier",
        "dispatch_path": "workflow|agent",
        "effort_applied": "<value>|null",
        "review_rounds": <int>,
        "escalated":     true|false
      }
    )

`dispatch_path` and `effort_applied` are not bookkeeping. On the plain agent
path, effort is inert (see `./references/tiers.md`), so a drawer that records
an effort it never applied poisons the dataset this table is meant to improve.
Write `null` when no effort was passed.
```

`## Finishing the Branch` halt contract, verbatim:

```markdown
A controller may instruct you to stop before this step. When it does: stop
after the final task's review and commit, report what was completed, and
create no pull request. Honoring that instruction is not optional — an
orchestrator that owns its own PR path (such as `/collab`) will treat a PR
created here as a protocol violation.
```

- [ ] **Step 4: Verify every authored file parses for both harnesses**

Run: `python3 scripts/sync_skills.py --check; echo "exit=$?"`
Expected: `exit=1` with a list of missing/stale files. **Not** `exit=2` — a 2 means `SkillSyncError`, i.e. a bad token or unclosed block that must be fixed before continuing.

- [ ] **Step 5: Confirm no dangling skill references remain**

Run: `grep -rn "superpowers\|brainstorming\|writing-plans\|subagent-driven-development\|executing-plans\|using-git-worktrees\|finishing-a-development-branch\|requesting-code-review\|test-driven-development\|using-superpowers" skills/`
Expected: no output. Any hit is a Global Constraint 9 violation.

- [ ] **Step 6: Commit**

```bash
git add skills/iron-build
git commit -m "feat(skills): author iron-build with tier lineup and prompts

Absorbs subagent-driven-development, executing-plans, using-git-worktrees,
finishing-a-development-branch, and requesting-code-review. The Codex-only
controller-owned loop is unified upward so Claude gets it too -- a deliberate
behavior change. tiers.md is honest about effort being inert on the plain
Agent-tool path and about Haiku rejecting effort entirely."
```

---

## Task 6: Generate, delete the eight vendored skills, add the tier-parity test

**Files:**
- Generate: `.claude-plugin/skills/iron-*/**`, `.codex-plugin/skills/iron-*/**`
- Delete: eight directories under each of `.claude-plugin/skills/` and `.codex-plugin/skills/`
- Modify: `.claude-plugin/skills/ATTRIBUTION.md`, `.codex-plugin/skills/ATTRIBUTION.md`
- Modify: `scripts/test_sync_skills.py` (append the parity test)

**Interfaces:**
- Consumes: `sync_skills.plan` / `write` (Tasks 1–2), all authored content (Tasks 3–5).
- Produces: committed generated trees that `check_skills_sync.py` (Task 7) compares against.

- [ ] **Step 1: Write the failing tier-parity test**

Byte comparison cannot catch this: `tiers.md` is *supposed* to differ per harness, so a tier present in `iron-plan`'s shared policy but missing from one harness's lineup would sail through the drift check. Append to `scripts/test_sync_skills.py`:

```python
class TierParityTests(unittest.TestCase):
    """Every tier in iron-plan's shared policy has a row in EVERY harness
    lineup. This is the one property the byte-for-byte drift check cannot
    cover, because tiers.md is meant to differ per harness.
    """

    TIERS = ("cheap", "standard", "deep", "frontier")

    def setUp(self) -> None:
        self.rendered = sync_skills.plan()

    def test_policy_names_exactly_the_declared_tiers(self) -> None:
        policy = self.rendered["claude"]["iron-plan/SKILL.md"]
        for tier in self.TIERS:
            self.assertIn(f"`{tier}`", policy, f"tier {tier} missing from iron-plan policy")

    def test_every_tier_has_a_row_in_every_harness_lineup(self) -> None:
        for harness, files in self.rendered.items():
            lineup = files["iron-build/references/tiers.md"]
            for tier in self.TIERS:
                self.assertIn(
                    f"| `{tier}` |",
                    lineup,
                    f"tier {tier} has no row in the {harness} lineup",
                )

    def test_claude_cheap_row_carries_no_effort_value(self) -> None:
        # Haiku 4.5 returns HTTP 400 when passed `effort`. The cheap row must
        # stay effort-free, and the generator must not synthesize one.
        lineup = self.rendered["claude"]["iron-build/references/tiers.md"]
        row = next(line for line in lineup.split("\n") if line.startswith("| `cheap` |"))
        self.assertNotIn("medium", row)
        self.assertNotIn("high", row)
        self.assertIn("unsupported", row)

    def test_claude_lineup_states_the_agent_tool_effort_caveat(self) -> None:
        lineup = self.rendered["claude"]["iron-build/references/tiers.md"]
        self.assertIn("no `effort` parameter", lineup)

    def test_iron_build_records_the_dispatch_path(self) -> None:
        for harness, files in self.rendered.items():
            skill = files["iron-build/SKILL.md"]
            self.assertIn("dispatch_path", skill, f"{harness} iron-build omits dispatch_path")

    def test_no_skill_references_an_uninstalled_skill(self) -> None:
        forbidden = (
            "superpowers",
            "brainstorming",
            "writing-plans",
            "subagent-driven-development",
            "executing-plans",
            "using-git-worktrees",
            "finishing-a-development-branch",
            "requesting-code-review",
            "test-driven-development",
            "using-superpowers",
        )
        for harness, files in self.rendered.items():
            for relative, body in files.items():
                for name in forbidden:
                    self.assertNotIn(
                        name,
                        body,
                        f"{harness}/{relative} references uninstalled skill {name!r}",
                    )
```

- [ ] **Step 2: Run it to verify it fails**

Run: `python3 scripts/test_sync_skills.py TierParityTests -v`
Expected: FAIL — at minimum `test_no_skill_references_an_uninstalled_skill` should pass already (Task 5 Step 5 proved it) while `test_policy_names_exactly_the_declared_tiers` and the lineup tests exercise files that now exist. If any fails, the authored content is wrong; fix `skills/`, not the test.

- [ ] **Step 3: Generate**

```bash
python3 scripts/sync_skills.py
```

Expected: a `wrote skills/iron-...` line per file, for both plugin roots.

- [ ] **Step 4: Delete the eight vendored skill directories from both plugin roots**

```bash
for root in .claude-plugin .codex-plugin; do
  git rm -r --quiet \
    "$root/skills/executing-plans" \
    "$root/skills/finishing-a-development-branch" \
    "$root/skills/requesting-code-review" \
    "$root/skills/subagent-driven-development" \
    "$root/skills/test-driven-development" \
    "$root/skills/using-git-worktrees" \
    "$root/skills/using-superpowers" \
    "$root/skills/writing-plans"
done
```

Verify `.codex-plugin/skills/pr-review-toolkit/` survived:

Run: `ls .codex-plugin/skills/`
Expected: `ATTRIBUTION.md  iron-build  iron-plan  iron-spec  iron-tdd  pr-review-toolkit`

Run: `ls .claude-plugin/skills/`
Expected: `ATTRIBUTION.md  iron-build  iron-plan  iron-spec  iron-tdd`

- [ ] **Step 5: Update both ATTRIBUTION.md files**

The MIT obligation survives the rewrite — the `iron-*` skills are derived works, not clean-room. Replace the bulleted list in `.claude-plugin/skills/ATTRIBUTION.md` (lines 6-14) with:

```markdown
The `iron-*` skills in this directory are derived works, generated from the
canonical source in `skills/`. They absorb and substantially rewrite skills
vendored from [obra/superpowers](https://github.com/obra/superpowers) by
Jesse Vincent, distributed under the MIT License:

- `iron-plan` derives from `writing-plans`
- `iron-build` derives from `subagent-driven-development`, `executing-plans`,
  `using-git-worktrees`, `finishing-a-development-branch`, and
  `requesting-code-review`
- `iron-tdd` derives from `test-driven-development`

`iron-spec` is authored fresh and derives from no upstream skill.
```

Apply the same replacement in `.codex-plugin/skills/ATTRIBUTION.md`, changing its opening line from "Most skills in this directory are vendored, modified copies" to match, and **leaving its `pr-review-toolkit` paragraph (lines 20-25) untouched** — that is a separate attribution to Anthropic and is still accurate.

Keep the MIT license text verbatim in both files. The generator never touches `ATTRIBUTION.md` (Task 2 pins this with `test_write_prunes_stale_iron_dirs_only`).

- [ ] **Step 6: Run the full generator self-test**

Run: `python3 scripts/test_sync_skills.py -v`
Expected: PASS, all tests including `TierParityTests`.

Run: `python3 scripts/sync_skills.py --check`
Expected: `generated skills match skills/`, exit 0.

- [ ] **Step 7: Commit**

```bash
git add -A skills .claude-plugin/skills .codex-plugin/skills scripts/test_sync_skills.py
git commit -m "feat(skills): generate iron-* trees, retire the eight vendored skills

2,404 authored lines (4,800 counting the duplicate) become ~1,030 authored
plus generated output. ATTRIBUTION.md keeps the MIT notice and now names the
derived works. Codex's pr-review-toolkit is untouched.

Adds the tier-parity test: byte comparison cannot check tiers.md, which is
supposed to differ per harness, so a tier added to the policy without both
lineups is now a test failure."
```

---

## Task 7: Drift guard — `check_skills_sync.py`, CI, and the git-hook surface

**Files:**
- Create: `scripts/check_skills_sync.py`
- Modify: `.github/workflows/ci.yml:61-62` (insert after the MCP wrapper drift check)
- Modify: `scripts/git_hook/manifest.py`
- Modify: `scripts/test_run_git_hook.py`

**Interfaces:**
- Consumes: `sync_skills.plan`, `sync_skills.diff`, `sync_skills.TARGETS` (Task 2); committed generated output (Task 6).
- Produces:
  - `manifest.is_skills_path(path: str) -> bool`
  - `manifest.SURFACE_SKILLS = "skills"`
  - `manifest.SKILLS_EXACT_PATHS: frozenset[str]`
  - a `Gate(name="skills_sync_check", ...)` in `GATES`

- [ ] **Step 1: Write `scripts/check_skills_sync.py`**

```python
#!/usr/bin/env python3
"""Drift guard: committed per-harness skills must match the canonical skills/.

Regenerates in memory and diffs against the committed trees. Hard gate, not a
warning: this is the guardrail whose absence let the Codex and Claude copies of
subagent-driven-development diverge by 127 lines without anyone noticing.

Exit codes: 0 clean, 1 drift, 2 unrenderable source.
"""
from __future__ import annotations

import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))

import sync_skills


def main() -> int:
    try:
        rendered = sync_skills.plan()
    except sync_skills.SkillSyncError as exc:
        detail = f"skills/ does not render: {exc}"
        print(f"::error title=skills sync::{detail}")
        print(f"check_skills_sync: {detail}", file=sys.stderr)
        return 2

    drifted = sync_skills.diff(rendered, sync_skills.TARGETS)
    if not drifted:
        print("check_skills_sync: OK — generated skills match skills/")
        return 0

    detail = (
        f"DRIFT: {len(drifted)} generated skill file(s) do not match skills/ "
        f"({', '.join(drifted[:5])}{', …' if len(drifted) > 5 else ''}). "
        "Run: python3 scripts/sync_skills.py"
    )
    print(f"::error title=skills sync::{detail}")
    for entry in drifted:
        print(f"  {entry}", file=sys.stderr)
    print("Run: python3 scripts/sync_skills.py", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
```

- [ ] **Step 2: Verify it passes clean and fails on drift**

```bash
python3 scripts/check_skills_sync.py; echo "clean exit=$?"
printf '\nDELIBERATE DRIFT\n' >> .claude-plugin/skills/iron-tdd/SKILL.md
python3 scripts/check_skills_sync.py; echo "drift exit=$?"
git checkout .claude-plugin/skills/iron-tdd/SKILL.md
python3 scripts/check_skills_sync.py; echo "restored exit=$?"
```

Expected: `clean exit=0`, then `drift exit=1` with an `::error title=skills sync::` line naming `skills/iron-tdd/SKILL.md (stale)`, then `restored exit=0`.

- [ ] **Step 3: Add the two CI steps**

In `.github/workflows/ci.yml`, immediately after the `MCP wrapper drift check` step (currently lines 61-62) and before `cargo fmt --check`:

```yaml
      - name: skills sync drift check
        run: python3 scripts/check_skills_sync.py

      - name: skills generator self-test
        run: python3 scripts/test_sync_skills.py
```

Both are stdlib-only and need no install step, consistent with every other Python self-test in that job except the pytest-based hook runner.

- [ ] **Step 4: Write the failing manifest tests**

Append to `scripts/test_run_git_hook.py`:

```python
def test_canonical_skills_source_classifies_skills():
    assert manifest.classify_path("skills/iron-build/SKILL.md") == manifest.SURFACE_SKILLS
    assert manifest.classify_path("skills/vocab.toml") == manifest.SURFACE_SKILLS


def test_generated_skills_classify_skills_in_every_plugin_root():
    for root in (".claude-plugin", ".codex-plugin"):
        path = f"{root}/skills/iron-build/references/tiers.md"
        assert manifest.classify_path(path) == manifest.SURFACE_SKILLS, path


def test_skills_surface_beats_the_generic_docs_catch_all():
    # skills/*.md would otherwise satisfy is_docs_path and select no gate.
    assert manifest.is_docs_path("skills/iron-tdd/SKILL.md") is True
    assert manifest.classify_path("skills/iron-tdd/SKILL.md") == manifest.SURFACE_SKILLS


def test_skills_lookalike_directories_do_not_match():
    # Byte-exact segment matching, same rule as docsite/ vs docs/.
    assert manifest.is_skills_path("skillset/notes.md") is False
    assert manifest.is_skills_path("docs/skills/notes.md") is False
    assert manifest.is_skills_path(".claude-plugin-backup/skills/x.md") is False


def test_generator_scripts_select_the_skills_gate():
    for path in manifest.SKILLS_EXACT_PATHS:
        assert manifest.classify_path(path) == manifest.SURFACE_SKILLS, path


def test_skills_exact_paths_covers_every_generator_script():
    # Mirrors test_hook_exact_paths_covers_every_git_hook_module: walk the
    # filesystem rather than trusting a hand-maintained list, so a new
    # generator helper cannot silently stop selecting its own gate.
    on_disk = {
        path.relative_to(ROOT).as_posix()
        for path in SCRIPTS.glob("*skills*.py")
    }
    missing = sorted(on_disk - manifest.SKILLS_EXACT_PATHS)
    assert not missing, (
        f"generator script(s) {missing} are not in SKILLS_EXACT_PATHS, so editing "
        "them would not select the skills sync gate"
    )
    stale = sorted(manifest.SKILLS_EXACT_PATHS - on_disk)
    assert not stale, f"SKILLS_EXACT_PATHS names non-existent script(s) {stale}"


def test_installer_stays_on_the_collab_surface():
    # Deliberate, not an oversight. scripts/install-ironmem.sh is in
    # COLLAB_EXACT_PATHS and is_collab_protocol_path is checked first, so
    # installer edits fire the collab template lint, NOT the skills gate.
    # That is correct: the skills gate checks generator-vs-output drift, which
    # an installer edit cannot cause. Reordering SURFACES to "fix" this would
    # steal the installer from the collab gate, which is the real regression.
    assert (
        manifest.classify_path("scripts/install-ironmem.sh")
        == manifest.SURFACE_COLLAB_PROTOCOL
    )


def test_skills_sync_gate_is_registered_for_both_phases():
    gate = next(g for g in manifest.GATES if g.name == "skills_sync_check")
    assert gate.argv == ("python3", "scripts/check_skills_sync.py")
    assert gate.surfaces == frozenset({manifest.SURFACE_SKILLS})
    assert gate.phases == frozenset({manifest.PHASE_PRE_COMMIT, manifest.PHASE_PRE_PUSH})
    assert gate.always is False
```

- [ ] **Step 5: Run them to verify they fail**

Run: `python3 scripts/test_run_git_hook.py -k skills -v`
Expected: FAIL — `AttributeError: module 'manifest' has no attribute 'SURFACE_SKILLS'`

- [ ] **Step 6: Make the four coordinated edits to `scripts/git_hook/manifest.py`**

**6a — exact-path set.** Add next to `HOOK_EXACT_PATHS` (after the block ending near line 46):

```python
# The generator, its guard, and its self-test. Editing any of them must select
# the skills sync gate: a change to the generator can silently alter every
# generated file, which is exactly the drift the gate exists to catch. Pinned
# by `test_skills_exact_paths_covers_every_generator_script`, which walks
# scripts/ rather than trusting this list.
SKILLS_EXACT_PATHS = frozenset({
    "scripts/sync_skills.py",
    "scripts/check_skills_sync.py",
    "scripts/test_sync_skills.py",
})
```

**6b — predicate.** Add immediately after `is_collab_protocol_path` (line 92-98), so it sits with the other specific classifiers:

```python
def is_skills_path(path: str) -> bool:
    """The workflow-skill surface: the canonical authored tree at `skills/`,
    the generated copies inside any harness plugin root, and the generator
    scripts themselves.

    Matched on `/`-split segments, never substrings -- `skillset/notes.md` and
    `docs/skills/notes.md` must not match, same byte-exact rule that keeps
    `docsite/` from matching `docs/`.

    Generated copies live under a gate-covered plugin root, where they would
    otherwise classify UNKNOWN and escalate to every gate. Claiming them here
    is deliberate: no `cargo test` assertion reads plugin-root `skills/`
    content (`packaging.rs`'s REQUIRED_ASSETS is bin/hooks/plugin.json;
    `plugin_metadata.rs` parses `agents/*.md`, not `skills/*.md`), so the
    skills sync gate is the gate that actually covers them.
    """
    if path in SKILLS_EXACT_PATHS:
        return True
    segments = path.split("/")
    if segments[0] == "skills":
        return True
    return (
        len(segments) >= 2
        and _is_gate_covered_plugin_segment(segments[0])
        and segments[1] == "skills"
    )
```

**6c — surface constant.** Add to the block at lines 274-278:

```python
SURFACE_SKILLS = "skills"
```

**6d — register in `SURFACES`.** Declaration order is check order and first match wins. `SURFACE_SKILLS` must be declared **before** `SURFACE_DOCS` (which would otherwise claim every `skills/**.md`) and after `SURFACE_COLLAB_PROTOCOL` (which must keep `scripts/install-ironmem.sh` — Global Constraint 3):

```python
SURFACES: MappingProxyType[str, Callable[[str], bool]] = MappingProxyType(
    {
        SURFACE_RUST_WORKSPACE: is_rust_path,
        SURFACE_COLLAB_PROTOCOL: is_collab_protocol_path,
        SURFACE_HOOK_SELF_TEST: is_hook_path,
        SURFACE_SKILLS: is_skills_path,
        SURFACE_DOCS: is_docs_path,
        SURFACE_INERT_CONFIG: is_inert_config_path,
    }
)
```

**6e — gate.** Append to `GATES` (declaration order is execution order; put it after `collab_template_lint` and before the three Rust gates, so a cheap Python check runs before `cargo`):

```python
    Gate(
        name="skills_sync_check",
        argv=("python3", "scripts/check_skills_sync.py"),
        phases=frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_SKILLS}),
        always=False,
    ),
```

- [ ] **Step 7: Update the two stale doc-path assertions**

`scripts/test_run_git_hook.py:416` and `:503` assert on `docs/superpowers/plans/notes.txt`. That path is retired in Task 9; the assertions are about the `docs` surface, not the directory name. Change both string literals to `docs/iron/plans/notes.txt`. Both must still classify `SURFACE_DOCS` — `docs/iron/` is under `docs/`, not under `skills/`, so `is_skills_path` does not claim it.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `python3 scripts/test_run_git_hook.py -v`
Expected: PASS, including the nine new tests. `Gate.__post_init__` validates the surface name at import time, so a typo in `SURFACE_SKILLS` fails collection with a named error rather than a later `KeyError`.

- [ ] **Step 9: Prove the gate actually fires**

```bash
python3 - <<'PY'
import sys, pathlib
sys.path.insert(0, str(pathlib.Path("scripts")))
from git_hook import manifest
cs = manifest.ChangeSet(paths=("skills/iron-build/SKILL.md",), unknown=False, reason=None)
print([g.name for g in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, cs)])
PY
```

Expected output includes `'skills_sync_check'` and excludes `'rust_test'`.

- [ ] **Step 10: Commit**

```bash
git add scripts/check_skills_sync.py .github/workflows/ci.yml scripts/git_hook/manifest.py scripts/test_run_git_hook.py
git commit -m "feat(skills): wire the sync drift guard into CI and the git hook

Adds SURFACE_SKILLS ahead of the generic docs catch-all, claims the generated
copies out of UNKNOWN-escalation (no cargo test assertion reads plugin-root
skills/), and pins the deliberate decision that install-ironmem.sh stays on
the collab surface -- the skills gate checks generator-vs-output drift, which
an installer edit cannot cause."
```

---

## Task 8: Installer manifest and provenance-gated cleanup

**Files:**
- Modify: `scripts/install-ironmem.sh:31-46` (manifests), `:214-250` (new function nearby), `:459-470` (call sites)
- Modify: `scripts/test_install_ironmem.py`

**Interfaces:**
- Consumes: the four generated skill directories (Task 6).
- Produces: `remove_legacy_skills(harness, target_root, base_root)` in the installer.

**Why a base snapshot is the right signal.** `install_skill_set()` writes `cp -R "$source" "$base"` at `install-ironmem.sh:235-237`, with `base_root` = `$CLAUDE_HOME/.ironmem-bases/skills` (`:459-468`). The snapshot's existence is proof ironmem installed that skill. Absence means the copy is the user's own, or a globally-installed upstream Superpowers — removing it would be destroying someone else's file. `~/.claude/skills/` is a flat namespace shared with any global Superpowers install, so this distinction is the whole reason the cleanup can be safe at all.

- [ ] **Step 1: Write the failing tests**

Append to `scripts/test_install_ironmem.py`, inside `InstallIronmemSelfTest`:

```python
    def _full_install_env(self, home: pathlib.Path) -> dict[str, str]:
        claude_home = home / "claude-home"
        codex_home = home / "codex-home"
        return {
            "CLAUDE_HOME": str(claude_home),
            "CODEX_HOME": str(codex_home),
            "CLAUDE_SKILLS_DIR": str(home / "claude-discovery" / "skills"),
            "CLAUDE_AGENTS_DIR": str(home / "claude-discovery" / "agents"),
            "CLAUDE_COMMANDS_DIR": str(home / "claude-discovery" / "commands"),
            "CLAUDE_PROMPTS_DIR": str(home / "claude-discovery" / "prompts"),
            "CODEX_SKILLS_DIR": str(home / "codex-discovery" / "skills"),
            "CODEX_COMMANDS_DIR": str(home / "codex-discovery" / "commands"),
            "CODEX_PROMPTS_DIR": str(home / "codex-discovery" / "prompts"),
        }

    def test_install_places_the_four_iron_skills(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            for root_key in ("CLAUDE_SKILLS_DIR", "CODEX_SKILLS_DIR"):
                root = pathlib.Path(env[root_key])
                for skill in ("iron-spec", "iron-plan", "iron-build", "iron-tdd"):
                    self.assertTrue(
                        (root / skill / "SKILL.md").is_file(),
                        f"{skill} missing from {root_key}",
                    )

    def test_superseded_skill_is_removed_when_a_base_snapshot_proves_ours(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            skills_dir = pathlib.Path(env["CLAUDE_SKILLS_DIR"])
            base_dir = pathlib.Path(env["CLAUDE_HOME"]) / ".ironmem-bases" / "skills"

            installed = skills_dir / "writing-plans"
            installed.mkdir(parents=True)
            (installed / "SKILL.md").write_text("ours\n", encoding="utf-8")
            snapshot = base_dir / "writing-plans"
            snapshot.mkdir(parents=True)
            (snapshot / "SKILL.md").write_text("ours\n", encoding="utf-8")

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            self.assertFalse(installed.exists(), "superseded skill was not removed")
            self.assertFalse(snapshot.exists(), "base snapshot was not removed")

    def test_user_owned_skill_is_kept_when_no_base_snapshot_exists(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            skills_dir = pathlib.Path(env["CLAUDE_SKILLS_DIR"])

            theirs = skills_dir / "writing-plans"
            theirs.mkdir(parents=True)
            (theirs / "SKILL.md").write_text("the user's own copy\n", encoding="utf-8")

            result = self.run_installer(
                home, home / ".claude.json", skip_skills=False, extra_env=env
            )

            self.assertTrue(theirs.is_file() or theirs.is_dir())
            self.assertEqual(
                (theirs / "SKILL.md").read_text(encoding="utf-8"),
                "the user's own copy\n",
                "installer modified a skill it did not install",
            )
            self.assertIn("writing-plans", result.stderr)
            self.assertIn("no ironmem base snapshot", result.stderr)

    def test_cleanup_covers_the_codex_side_too(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            home = pathlib.Path(directory)
            env = self._full_install_env(home)
            skills_dir = pathlib.Path(env["CODEX_SKILLS_DIR"])
            base_dir = pathlib.Path(env["CODEX_HOME"]) / ".ironmem-bases" / "skills"

            for name in ("using-superpowers", "executing-plans"):
                (skills_dir / name).mkdir(parents=True)
                (skills_dir / name / "SKILL.md").write_text("ours\n", encoding="utf-8")
                (base_dir / name).mkdir(parents=True)
                (base_dir / name / "SKILL.md").write_text("ours\n", encoding="utf-8")

            self.run_installer(home, home / ".claude.json", skip_skills=False, extra_env=env)

            for name in ("using-superpowers", "executing-plans"):
                self.assertFalse((skills_dir / name).exists(), f"{name} survived on the Codex side")
            self.assertTrue((skills_dir / "pr-review-toolkit" / "SKILL.md").is_file())
```

- [ ] **Step 2: Run them to verify they fail**

Run: `python3 scripts/test_install_ironmem.py -v`
Expected: FAIL — `test_install_places_the_four_iron_skills` fails because `validate_packaged_skills` still asserts the eight old names and the installer exits 1 with `ERROR: bundled Codex skill missing: .../writing-plans/SKILL.md`.

- [ ] **Step 3: Replace the skill manifests at `scripts/install-ironmem.sh:31-46`**

```bash
REQUIRED_SHARED_SKILLS=(
  iron-spec
  iron-plan
  iron-build
  iron-tdd
)

# Skills earlier ironmem versions installed, superseded by the iron-* set.
# Removed on upgrade -- but ONLY where a base snapshot under
# <harness home>/.ironmem-bases/skills/<name> proves ironmem wrote that copy.
# ~/.claude/skills and ~/.codex/skills are flat namespaces shared with any
# globally-installed upstream Superpowers, so a missing snapshot means the
# directory is someone else's and must be left alone.
LEGACY_SHARED_SKILLS=(
  writing-plans
  subagent-driven-development
  finishing-a-development-branch
  executing-plans
  using-git-worktrees
  using-superpowers
  requesting-code-review
  test-driven-development
)

REQUIRED_CODEX_SKILLS=(
  pr-review-toolkit
)

REQUIRED_CLAUDE_SKILLS=()
```

`REQUIRED_CODEX_SKILLS` and `REQUIRED_CLAUDE_SKILLS` are unchanged; they are restated so the whole manifest block reads as one unit.

- [ ] **Step 4: Add `remove_legacy_skills()` after `install_skill_set()` (ends near line 250)**

```bash
# Remove skills superseded by the iron-* set, using the three-way-merge base
# snapshot as the provenance signal. Present => install_skill_set() wrote that
# copy, so removing it is removing our own file. Absent => it is the user's own
# copy (or an upstream Superpowers install sharing this flat namespace), and we
# warn instead of deleting.
#
# Runs AFTER install_skill_set() so migrate_legacy_base() has already relocated
# any older base layout -- otherwise a legitimately-ours skill whose snapshot
# had not yet been migrated would look user-owned and survive forever.
remove_legacy_skills() {
  local harness="$1"
  local target_root="$2"
  local base_root="$3"

  for skill in "${LEGACY_SHARED_SKILLS[@]}"; do
    local target="$target_root/$skill"
    local base="$base_root/$skill"

    [[ -e "$target" || -L "$target" ]] || continue

    if [[ ! -d "$base" ]]; then
      echo "    WARN: keeping $harness skill '$skill' at $target — no ironmem base snapshot, so it is not ours to remove" >&2
      continue
    fi

    rm -rf "$target"
    rm -rf "$base"
    echo "    removed superseded $harness skill $skill"
  done
}
```

- [ ] **Step 5: Call it at both call sites**

In `scripts/install-ironmem.sh`, immediately after the `install_skill_set "Claude" ... "${REQUIRED_CLAUDE_SKILLS[@]}"` conditional block (around line 470) and before `install_agent_set`:

```bash
  remove_legacy_skills "Codex" "$CODEX_SKILLS_DIR" "$CODEX_HOME/.ironmem-bases/skills"
  remove_legacy_skills "Claude" "$CLAUDE_SKILLS_DIR" "$CLAUDE_HOME/.ironmem-bases/skills"
```

Both calls sit inside the `if [[ "$SKIP_SKILLS" -eq 0 ]]` block, so `--skip-skills` skips cleanup too — a run that installs nothing must remove nothing.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `python3 scripts/test_install_ironmem.py -v`
Expected: PASS, including the four new tests. (Skips entirely if `jq` is absent — check the output says `OK`, not `skipped`.)

- [ ] **Step 7: Commit**

```bash
git add scripts/install-ironmem.sh scripts/test_install_ironmem.py
git commit -m "feat(install): ship iron-* skills, retire the superseded eight

Cleanup is gated on the three-way-merge base snapshot: present means ironmem
installed that copy and removal is safe; absent means it belongs to the user
or to a global Superpowers install sharing the flat ~/.claude/skills namespace,
and we warn instead. Both branches are tested, on both harnesses."
```

---

## Task 9: Move `docs/superpowers/` into `docs/iron/`

**Files:**
- Move: 6 files from `docs/superpowers/{plans,specs}/` to `docs/iron/{plans,specs}/`
- Modify: `.gitignore:15-17`
- Modify: `scripts/check_collab_turn_templates.py:133`
- Modify: `.claude-plugin/prompts/collab-turn-plan-finalize.md:34,36`
- Modify: `.claude-plugin/commands/collab.md:300,345`
- Modify: `README.md:867`
- Modify: `crates/ironmem/src/mcp/server.rs:1120`, `crates/ironmem/src/mcp/tools/handoff.rs:760,777`, `crates/ironmem/src/mcp/tools/collab_events.rs:401,409`, `crates/ironmem/src/mcp/tools/collab_session.rs:2908,2920,2985,2998`

**Interfaces:**
- Consumes: the `docs/iron/plans/` convention written into `iron-plan` (Task 3).
- Produces: a single tracked location for specs and plans.

### Decision required, made here: `.gitignore`

`.gitignore:17` currently ignores `docs/superpowers/`, with a comment declaring those files "per-session and developer-facing, not durable repo content." But two of the six are already tracked, and `docs/iron/specs/` is fully tracked (this plan's own design doc lives there). The directory is half-tracked today.

**This plan removes the ignore rule and tracks everything under `docs/iron/`.** Rationale: `iron-spec` and `iron-plan` now write to `docs/iron/`, and the whole point of this spec is that a half-maintained duplicate surface is where drift comes from. A directory that is tracked for specs and ignored for plans reproduces exactly that. The cost is that plan documents become durable repo content and show in diffs.

If the maintainer prefers the old policy, the one-line alternative is to replace the removed line with `docs/iron/plans/` — but then `iron-plan`'s output is invisible to review, and this very file would be untracked. Flag it before Step 2 if that is the preference; everything downstream is unaffected either way.

- [ ] **Step 1: Move the two tracked files with `git mv`, preserving history**

```bash
mkdir -p docs/iron/plans
git mv docs/superpowers/plans/2026-07-26-review-diff-compression.md docs/iron/plans/
git mv docs/superpowers/specs/2026-07-26-review-diff-compression-design.md docs/iron/specs/
```

- [ ] **Step 2: Move the four untracked files with plain `mv`**

`git mv` cannot move them — they are ignored and unknown to the index.

```bash
mv docs/superpowers/plans/2026-07-22-collab-token-baseline.md docs/iron/plans/
mv docs/superpowers/plans/2026-07-27-hybrid-prompt-recall.md docs/iron/plans/
mv docs/superpowers/plans/2026-07-29-mcp-response-compression.md docs/iron/plans/
mv docs/superpowers/specs/2026-07-23-remove-ironmem-bases-prefix-design.md docs/iron/specs/
rmdir docs/superpowers/plans docs/superpowers/specs docs/superpowers
```

Verify: `ls docs/superpowers 2>&1`
Expected: `ls: docs/superpowers: No such file or directory`

Verify: `ls docs/iron/plans docs/iron/specs`
Expected: 5 plans (4 moved plus this file) and 4 specs.

- [ ] **Step 3: Remove the ignore rule**

Delete lines 15-17 of `.gitignore`:

```
# Superpowers working artifacts (writing-plans output, /collab bridges,
# design specs). Per-session and developer-facing, not durable repo content.
# The canonical protocol/architecture lives in docs/COLLAB.md and the
# in-tree Rust source. Any historical file is retrievable from git history.
docs/superpowers/
```

Verify: `git check-ignore -v docs/iron/plans/2026-07-22-collab-token-baseline.md; echo "exit=$?"`
Expected: `exit=1` (not ignored).

- [ ] **Step 4: Update the collab template lint literal**

`scripts/check_collab_turn_templates.py:133` requires this exact string inside `collab-turn-plan-finalize.md`:

```python
        "docs/superpowers/plans/YYYY-MM-DD-<short-feature>.md",
```

Change to:

```python
        "docs/iron/plans/YYYY-MM-DD-<short-feature>.md",
```

Then update both occurrences in `.claude-plugin/prompts/collab-turn-plan-finalize.md` (lines 34 and 36) to match. **The lint and the template must change in the same commit** — the lint asserts the literal is present, so changing either alone is a hard failure.

Run: `python3 scripts/check_collab_turn_templates.py`
Expected: exit 0.

- [ ] **Step 5: Update the two remaining `/collab` prose references**

`.claude-plugin/commands/collab.md:300` and `:345` both say `docs/superpowers/plans/...`. Change both to `docs/iron/plans/...`. Line 345 additionally says "Superpowers-compatible task markdown" — leave that phrasing for Stage 2; Stage 1 only fixes paths that would now point at a directory that does not exist.

- [ ] **Step 6: Fix the dangling README reference**

`README.md:867` points at `docs/superpowers/specs/2026-04-30-pref-enrich-experiment-retro.md`, which was never committed (it was under the ignore rule) and does not exist in the working tree either. Moving the directory does not fix it. Replace the sentence:

> The pref-enrich experiment did not meet its target lift on LongMemEval — see `docs/superpowers/specs/2026-04-30-pref-enrich-experiment-retro.md`.

with:

> The pref-enrich experiment did not meet its target lift on LongMemEval; the retro was a developer-local document and is not in-tree.

- [ ] **Step 7: Update Rust doc comments and test fixture strings**

These are one doc comment and eight string literals in tests. Mechanical:

```bash
grep -rl "docs/superpowers/" crates/ | xargs sed -i '' 's|docs/superpowers/|docs/iron/|g'
grep -rn "docs/superpowers" crates/
```

Expected: the second command prints nothing.

**Do not touch `benchmarks/`.** `benchmarks/provbench/SPEC.md`, the two findings documents, `phase1/src/rules/r3_python_resolver.rs`, `phase1/src/rules/r4_span_hash_changed.rs`, `phase1/tests/*`, and `abeval/tests/collab_driver_loop.rs` cite `docs/superpowers/...` paths as **historical record** — they name where a design doc lived when a benchmark round was run. Rewriting them would falsify provenance in a document whose entire purpose is provenance. Leave them.

- [ ] **Step 8: Verify nothing live still points at the old path**

```bash
grep -rn "docs/superpowers" --include='*.md' --include='*.py' --include='*.sh' --include='*.rs' --include='*.yml' . \
  | grep -v '^./benchmarks/' | grep -v '^./docs/iron/specs/2026-07-29-lean-workflow-layer'
```

Expected: no output. (The two Stage-1 spec documents legitimately discuss the old path and are excluded.)

- [ ] **Step 9: Run the affected checks**

```bash
python3 scripts/check_collab_turn_templates.py
python3 scripts/test_run_git_hook.py -q
cargo test --workspace
```

Expected: all pass. `cargo test` covers the Rust literal changes from Step 7.

- [ ] **Step 10: Commit**

```bash
git add -A .gitignore docs/iron README.md crates scripts/check_collab_turn_templates.py \
        .claude-plugin/prompts/collab-turn-plan-finalize.md .claude-plugin/commands/collab.md
git commit -m "refactor(docs): move docs/superpowers to docs/iron

Two of six files were tracked and moved with git mv; the other four were
ignored working artifacts moved with plain mv. Drops the docs/superpowers/
ignore rule so specs and plans are tracked uniformly -- docs/iron/specs was
already tracked, and a half-tracked directory is the drift this spec exists
to remove.

benchmarks/ references are deliberately untouched: they cite where design
docs lived when a round was run, and rewriting them would falsify provenance.

Also fixes a README link to a retro that was never in-tree."
```

---

## Task 10: Repoint `/collab` and `/evaluate-issue` at the new skills

**Files:**
- Modify: `.claude-plugin/commands/collab.md:462-500`
- Modify: `.claude-plugin/prompts/collab-turn-code-implement.md:66-77`
- Modify: `.codex-plugin/prompts/collab-batch-impl.md:105,116,119`
- Modify: `.claude-plugin/commands/evaluate-issue.md:2,176`
- Modify: `.codex-plugin/prompts/evaluate-issue.md:2,176-177`

**Interfaces:**
- Consumes: `iron-build`'s `## Finishing the Branch` heading and its halt contract (Task 5); `iron-plan` and `iron-tdd` by name (Tasks 3–4).
- Produces: nothing downstream. This is the last task.

This is the minimum needed for "everything still works". The 1,123-line `collab.md` trim is Stage 2.

- [ ] **Step 1: Repoint `.claude-plugin/commands/collab.md`**

Three substitutions in the `implementer == "claude"` branch (lines 462-490):

| Line | From | To |
|---|---|---|
| 462 | `` invokes `Skill('subagent-driven-development')` on `plan_file_path` `` | `` invokes `Skill('iron-build')` on `plan_file_path` `` |
| 468-469 | ``**Hard stop at the boundary before `finishing-a-development-branch`.** That sub-skill prompts the user`` | ``**Hard stop at the boundary before `iron-build`'s *Finishing the Branch* step.** That step prompts the user`` |
| 474-477 | ``Before invoking `subagent-driven-development`, the worker tells its controller-loop … do *not* invoke `finishing-a-development-branch`."`` | ``Before invoking `iron-build`, the worker tells its controller-loop … do *not* run the *Finishing the Branch* step."`` |
| 482 | ``does not mention PR creation or `finishing-a-development-branch``` | ``does not mention PR creation or the *Finishing the Branch* step`` |
| 486 | ``PR creation/`finishing-a-development-branch``` | ``PR creation / the *Finishing the Branch* step`` |

And in the `implementer == "codex"` branch (lines 498-505):

| Line | From | To |
|---|---|---|
| 500 | ``run its own `subagent-driven-development` end-to-end (with the same `finishing-a-development-branch` carve-out`` | ``run its own `iron-build` end-to-end (with the same *Finishing the Branch* carve-out`` |
| 505 | ``Do *not* invoke `subagent-driven-development` locally`` | ``Do *not* invoke `iron-build` locally`` |

The `skill_overran_pr_boundary: <pr_number>` failure code at line 489 is unchanged — the protocol wire format does not move.

- [ ] **Step 2: Repoint `.claude-plugin/prompts/collab-turn-code-implement.md`**

| Line | From | To |
|---|---|---|
| 66 | ``Invoke `Skill('subagent-driven-development')` on `plan_file_path``` | ``Invoke `Skill('iron-build')` on `plan_file_path``` |
| 71 | ``STOP before `finishing-a-development-branch` (no PR here).`` | ``STOP before `iron-build`'s *Finishing the Branch* step (no PR here).`` |
| 73 | ``did not invoke `finishing-a-development-branch`,`` | ``did not run the *Finishing the Branch* step,`` |
| 77 | ``PR creation/`finishing-a-development-branch`;`` | ``PR creation / the *Finishing the Branch* step;`` |

- [ ] **Step 3: Repoint `.codex-plugin/prompts/collab-batch-impl.md` (the third invocation point)**

| Line | From | To |
|---|---|---|
| 105 | ``  `subagent-driven-development` or spawn an agent`` | ``  `iron-build` or spawn an agent`` |
| 116 | ``  `subagent-driven-development`. It must complete every task, review it,`` | ``  `iron-build`. It must complete every task, review it,`` |
| 119 | ``  `finishing-a-development-branch`: Claude owns PR creation at final review.`` | ``  the *Finishing the Branch* step: Claude owns PR creation at final review.`` |

- [ ] **Step 4: Repoint both `evaluate-issue` files**

`.claude-plugin/commands/evaluate-issue.md`:

| Line | From | To |
|---|---|---|
| 2 (frontmatter `description`) | `SUPERPOWERS (writing-plans → subagent-driven-development)` | `IRON (iron-plan → iron-build)` |
| 2 | `DIRECT (/plan + TDD)` | `DIRECT (/plan + iron-tdd)` |
| 176 | `` | SUPERPOWERS | Invoke the `writing-plans` skill on the issue spec; it flows into `subagent-driven-development`. | `` | `` | IRON | Invoke the `iron-plan` skill on the issue spec; it flows into `iron-build`. | `` |

`.codex-plugin/prompts/evaluate-issue.md`:

| Line | From | To |
|---|---|---|
| 2 | `SUPERPOWERS (writing-plans → subagent-driven-development)` | `IRON (iron-plan → iron-build)` |
| 176 | `` | DIRECT | Invoke the `test-driven-development` skill directly. | `` | `` | DIRECT | Invoke the `iron-tdd` skill directly. | `` |
| 177 | `` | SUPERPOWERS | Invoke the `writing-plans` skill … `subagent-driven-development`. | `` | `` | IRON | Invoke the `iron-plan` skill … `iron-build`. | `` |

If the verdict token `SUPERPOWERS` appears elsewhere in either file (a scoring table, a verdict enum), rename every occurrence to `IRON` in the same pass — a verdict the routing table no longer lists is a dead branch.

Run: `grep -n "SUPERPOWERS" .claude-plugin/commands/evaluate-issue.md .codex-plugin/prompts/evaluate-issue.md`
Expected: no output.

- [ ] **Step 5: Verify no shipped file references an uninstalled skill**

```bash
grep -rn "subagent-driven-development\|writing-plans\|executing-plans\|using-git-worktrees\|finishing-a-development-branch\|requesting-code-review\|test-driven-development\|using-superpowers\|brainstorming" \
  .claude-plugin/ .codex-plugin/ scripts/ \
  | grep -v "ATTRIBUTION.md" | grep -v "install-ironmem.sh" | grep -v "pr-review-toolkit"
```

Expected: no output.

Two exclusions are correct and intentional: `ATTRIBUTION.md` names the upstream skills as the origin of derived works (an MIT obligation), and `install-ironmem.sh`'s `LEGACY_SHARED_SKILLS` names them precisely so it can remove them.

`docs/COLLAB.md` still describes the old skill by name in ~10 places. That file is protocol documentation, not an executable surface, and rewriting it is Stage 2's job. Note it in the commit message rather than half-editing it here.

- [ ] **Step 6: Run every gate**

```bash
python3 scripts/check_skills_sync.py
python3 scripts/test_sync_skills.py
python3 scripts/check_collab_turn_templates.py
python3 scripts/test_run_git_hook.py -q
python3 scripts/test_install_ironmem.py
cargo test --workspace
```

Expected: all exit 0. Report the actual output — do not claim green without seeing it.

- [ ] **Step 7: Full-loop verification against a real install**

```bash
bash scripts/install-ironmem.sh --skip-build
ls ~/.claude/skills/ | grep -E 'iron-|writing-plans|subagent-driven'
ls ~/.codex/skills/ | grep -E 'iron-|writing-plans|subagent-driven'
```

Expected: four `iron-*` directories on each side, and none of the old eight — *unless* a warning line named one as user-owned, in which case that one legitimately survives and the warning is the proof. Read the installer's stderr before concluding.

- [ ] **Step 8: Commit**

```bash
git add .claude-plugin/commands/collab.md .claude-plugin/prompts/collab-turn-code-implement.md \
        .codex-plugin/prompts/collab-batch-impl.md \
        .claude-plugin/commands/evaluate-issue.md .codex-plugin/prompts/evaluate-issue.md
git commit -m "feat(collab): repoint invocation points at iron-build

Three executable invocation points, not two: the Codex-side
collab-batch-impl.md drives the same skill and would have broken the moment
the installer stopped shipping it. The stop-boundary language now names
iron-build's 'Finishing the Branch' section, whose halt contract is documented
in the skill itself.

/evaluate-issue's SUPERPOWERS verdict becomes IRON so it stops recommending
skills that no longer exist.

docs/COLLAB.md still names the old skills in prose; that rewrite is Stage 2."
```

---

## Verification

Stage 1 is complete when all of the following hold, each confirmed by running the command and reading the output:

| Check | Command | Expected |
|---|---|---|
| Generator self-test | `python3 scripts/test_sync_skills.py` | OK |
| Generated output current | `python3 scripts/check_skills_sync.py` | `OK — generated skills match skills/` |
| Drift is caught | append a line to a generated file, re-run the above | exit 1 with `::error title=skills sync::` |
| Git hook wiring | `python3 scripts/test_run_git_hook.py` | OK, incl. the nine new tests |
| Gate fires on skills edits | Task 7 Step 9 snippet | includes `skills_sync_check` |
| Installer | `python3 scripts/test_install_ironmem.py` | OK (not "skipped") |
| Collab template lint | `python3 scripts/check_collab_turn_templates.py` | exit 0 |
| Rust workspace | `cargo test --workspace` | pass |
| No dangling skill refs | Task 10 Step 5 grep | no output |
| No dangling doc paths | Task 9 Step 8 grep | no output |
| Real install | Task 10 Step 7 | four `iron-*`, old eight gone or warned about |

**Line-count claim, for the commit that closes the branch:**

```bash
find skills -name '*.md' | xargs wc -l | tail -1   # authored, target ~1,030
```

Baseline for comparison is 2,404 authored lines across `.claude-plugin/skills/` (1,510 in eight `SKILL.md` + 894 in support files), duplicated into `.codex-plugin/skills/`. If the authored total lands materially above ~1,030, say so plainly rather than restating the design's target.

---

## Self-Review

**Spec coverage.** Every section of the design doc maps to a task: canonical source + vocab (T1), generator (T1–T2), four skills (T3–T5), tier tables and both routing rules (T3, T5), parity mechanism hard errors (T1), generated header (T2), enforcement in CI and hooks (T7), installer manifest + provenance cleanup (T8), `docs/` rename (T9), `/collab` repoint (T10). The design's five testing rows all appear: `test_sync_skills.py` (T1–T2), `check_skills_sync.py` (T7), tier-parity (T6), extended `test_install_ironmem.py` with both cleanup branches (T8). The fifth row, `check_collab_turn_templates.py`, is marked Stage 2 in the design; T9 Step 4 touches it only for the path literal, which Stage 1 forces.

Every error-handling row is placed: `BLOCKED`/`NEEDS_CONTEXT` and reviewer rejection in `iron-build`'s status section (T5); unknown tier as a hard parse error in `iron-plan` and `tiers.md` (T3, T5); `cheap` context ceiling in `iron-plan`'s policy (T3); stale output as CI failure (T7); missing tier as an installer/test failure — implemented as the tier-parity test (T6) rather than extending `validate_packaged_skills()`, since that shell function checks for `SKILL.md` existence and cannot read a Markdown table; `frontier` 400 fallback in `tiers.md` (T5).

**Deviations from the source documents, all deliberate and all flagged inline:**

1. `git mv` works for 2 of 6 documents, not 6 — see *Two findings*, finding A, and T9 Steps 1–2.
2. A `.gitignore` policy decision is unavoidable and is made in T9 with the alternative stated.
3. Three executable `/collab` invocation points, not two — finding B, T10 Step 3.
4. `/evaluate-issue` (both harnesses) also names retired skills — T10 Step 4.
5. `benchmarks/` references to `docs/superpowers/` are deliberately **not** rewritten — T9 Step 7.
6. `README.md:867` points at a retro that was never committed; the link is removed rather than repointed — T9 Step 6.

**Type and name consistency.** `render`, `plan`, `write`, `diff`, `inject_header`, `load_vocab`, `SkillSyncError`, `GENERATED_HEADER`, `HARNESSES`, `TARGETS`, `OWNED_PREFIX` are defined in T1–T2 and used under those exact names in T6 and T7. `SURFACE_SKILLS`, `is_skills_path`, `SKILLS_EXACT_PATHS`, and the gate name `skills_sync_check` are defined in T7 Step 6 and asserted under those names in T7 Step 4. `remove_legacy_skills` and `LEGACY_SHARED_SKILLS` are defined in T8 Steps 3–4 and called in Step 5. The tier names `cheap|standard|deep|frontier` are identical in `iron-plan` (T3), `tiers.md` (T5), and `TierParityTests.TIERS` (T6). The section heading `## Finishing the Branch` is defined in T5 and referenced by that exact spelling throughout T10.

**Prose authoring in T3–T5.** Those tasks specify frontmatter, required section headings, per-section acceptance criteria, the exact source ranges to compress, and verbatim text for every load-bearing passage (tier policy, both routing rules, controller-owned loop, status handling, drawer schema, halt contract, `tiers.md` in full). What is left to the implementer is compression of known source prose against stated criteria, with two greps and a parity test as the gate. Reproducing all ~1,030 authored lines here would make the plan the artifact rather than the plan.
