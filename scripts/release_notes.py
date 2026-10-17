#!/usr/bin/env python3
"""Print one version's CHANGELOG.md section: the body of its GitHub release.

``release.yml`` runs this twice per tag:
  * in each build job, before compiling, so a tag whose version has no
    ``## [<version>]`` section fails in seconds instead of after the builds;
  * in the publish job, writing the section to the release's ``body_path``.

The section is everything below its ``## [<version>] - <date>`` heading up to
the next ``## [`` heading, with surrounding blank lines trimmed. A missing or
empty section exits 1. GitHub caps a release body at 125,000 characters, so a
longer section is cut at a line boundary and ends with a link to the full
CHANGELOG at that tag.

Usage:
  python3 scripts/release_notes.py v0.6.0              # print to stdout
  python3 scripts/release_notes.py v0.6.0 --out notes.md

Stdlib only (mirrors scripts/check_site_readme_sync.py).
"""
from __future__ import annotations

import argparse
import os
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent

# GitHub rejects a release body longer than this many characters.
BODY_LIMIT = 125_000


def extract_section(changelog: str, version: str) -> str | None:
    """Return the trimmed body of ``## [version]``, or None if absent or empty."""
    heading = re.compile(rf"## \[{re.escape(version)}\](?:\s|$)")
    lines = changelog.splitlines()
    for index, line in enumerate(lines):
        if not heading.match(line):
            continue
        body = []
        for line in lines[index + 1 :]:
            if line.startswith("## ["):
                break
            body.append(line)
        text = "\n".join(body).strip()
        return f"{text}\n" if text else None
    return None


def fit_release_body(body: str, full_changelog_url: str, limit: int = BODY_LIMIT) -> str:
    """Cut ``body`` at a line boundary so it, plus a pointer to the rest, fits ``limit``."""
    if len(body) <= limit:
        return body
    note = (
        "\n\n_Truncated to fit GitHub's release size limit; the full entry is in "
        f"[CHANGELOG.md]({full_changelog_url})._\n"
    )
    budget = limit - len(note)
    cut = body.rfind("\n", 0, budget)
    return body[: cut if cut > 0 else budget].rstrip() + note


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("version", help="release version, with or without a leading v")
    parser.add_argument("--changelog", type=pathlib.Path, default=ROOT / "CHANGELOG.md")
    parser.add_argument("--out", type=pathlib.Path, help="write here instead of stdout")
    args = parser.parse_args(argv)

    version = args.version.removeprefix("v")
    section = extract_section(args.changelog.read_text(encoding="utf-8"), version)
    if section is None:
        print(
            f"error: {args.changelog} has no non-empty `## [{version}]` section; "
            f"add one before tagging v{version}",
            file=sys.stderr,
        )
        return 1

    repo = os.environ.get("GITHUB_REPOSITORY", "ironrace/ironmem")
    body = fit_release_body(section, f"https://github.com/{repo}/blob/v{version}/CHANGELOG.md")
    if args.out:
        args.out.write_text(body, encoding="utf-8")
    else:
        sys.stdout.write(body)
    return 0


if __name__ == "__main__":
    sys.exit(main())
