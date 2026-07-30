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


def _label_root(root: pathlib.Path) -> str:
    """Repo-relative label for a target root, e.g. '.claude-plugin/skills'.

    Both `.claude-plugin/skills` and `.codex-plugin/skills` share the leaf
    name `skills`, so `root.name` alone can't tell them apart in log/diff
    output -- exactly the ambiguity this subsystem exists to catch. Targets
    passed in tests are temp directories outside ROOT, so fall back to an
    unambiguous two-component label rather than raising.
    """
    try:
        return root.relative_to(ROOT).as_posix()
    except ValueError:
        return f"{root.parent.name}/{root.name}"


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
        label_root = _label_root(root)
        for relative, body in sorted(files.items()):
            destination = root / relative
            destination.parent.mkdir(parents=True, exist_ok=True)
            if not destination.exists() or destination.read_text(encoding="utf-8") != body:
                destination.write_text(body, encoding="utf-8", newline="\n")
                changed.append(f"{label_root}/{relative}")

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
                    changed.append(f"{label_root}/{relative} (removed)")
            for path in sorted(root.rglob("*"), reverse=True):
                if path.is_dir() and path.name.startswith(OWNED_PREFIX) and not any(path.iterdir()):
                    path.rmdir()
    return changed


def diff(rendered: dict[str, dict[str, str]], targets: dict[str, pathlib.Path]) -> list[str]:
    """Relative paths whose committed content differs from `rendered`."""
    drifted: list[str] = []
    for harness, files in rendered.items():
        root = targets[harness]
        label_root = _label_root(root)
        for relative, body in sorted(files.items()):
            destination = root / relative
            if not destination.is_file():
                drifted.append(f"{label_root}/{relative} (missing)")
            elif destination.read_text(encoding="utf-8") != body:
                drifted.append(f"{label_root}/{relative} (stale)")
        if root.is_dir():
            expected = set(files)
            for path in sorted(root.rglob("*")):
                if path.is_file():
                    relative = path.relative_to(root).as_posix()
                    if relative.startswith(OWNED_PREFIX) and relative not in expected:
                        drifted.append(f"{label_root}/{relative} (orphaned)")
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
