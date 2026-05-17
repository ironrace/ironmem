#!/usr/bin/env python3
"""Independent auto-filter for the ProvBench Python labeler §9.1 spot-check.

This is the Python-corpus counterpart of `autofilter.py` (which targets the
Rust labeler against ripgrep). Same triage scheme, same I/O contract, same
output columns — only the language-specific re-derivation logic differs.

Goal — implement a minimal, labeler-independent re-derivation of each row's
expected label using only `git cat-file` and regex against the pilot flask
clone. Triage tags:

  GREEN     auto-derived label matches predicted_label with HIGH confidence;
            the row can be fast-tracked (human_label = predicted_label).
  YELLOW    auto-derived label is ambiguous or partially matches; surfaced
            to the human reviewer with the heuristic note.
  DISAGREE  auto-derived label clearly differs from predicted_label;
            surfaced to the human reviewer with both labels + note.
  UNCERTAIN the auto-filter could not decide (e.g. regex didn't bite, file
            absent at T₀, etc.); surfaced to the human reviewer.

The classifiers intentionally re-implement the fact-checks from scratch
rather than reusing labeler code, so this script can serve as an
independent control per SPEC §9.1. False matches on the GREEN path would
dilute the agreement metric, so green-path checks are deliberately strict;
when in doubt, the row escalates to a human reviewer.

Differences from the Rust autofilter
------------------------------------
* Fact ids use dotted Python qnames (e.g. ``src.flask.app.Flask.run``)
  rather than ``::``-separated Rust paths. The leaf is the last
  ``.``-segment.
* Field facts encode the container class as the qname's penultimate
  segment (e.g. ``src.flask.app.Flask.secret_key`` → container
  ``Flask``, field ``secret_key``).
* TestAssertion qnames are NOT prefixed with the dotted module — they
  are bare ``ClassName.test_fn`` or just ``test_fn`` — and the line
  number points at the assertion body, not the ``def``.
* The labeler operates on a single tree (T₀ = the corpus snapshot) and
  the spot-check CSV exclusively contains ``valid`` predictions at T₀,
  so the classifier only needs to verify presence — not derive a
  before/after diff. Stale verdicts still emit if a recorded line/symbol
  is missing from the recorded file (e.g. labeler bug).

Usage::

    python3 benchmarks/provbench/spotcheck/tools/autofilter_python.py \\
        --csv benchmarks/provbench/results/python-labeler-2026-05-15-spotcheck.csv \\
        --repo benchmarks/provbench/work/flask \\
        --t0 2f0c62f5e6e290843f03c1fa70817c7a3c7fd661 \\
        --out benchmarks/provbench/results/python-labeler-2026-05-15-spotcheck-autofilter.csv

Output is the input CSV widened by two columns: ``auto_tag`` and
``auto_note``. Downstream, ``fill_human_labels.py`` reads those columns to
fill canonical ``human_label`` values.
"""

from __future__ import annotations

import argparse
import csv
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Optional


T0_DEFAULT = "2f0c62f5e6e290843f03c1fa70817c7a3c7fd661"


# ---------------------------------------------------------------------
# git plumbing
# ---------------------------------------------------------------------


def git_cat_file(repo: Path, sha: str, path: str) -> Optional[bytes]:
    """Return the bytes of `<sha>:<path>` or None if the path doesn't
    exist in that tree. Never raises on missing-blob — that's a normal
    signal we use to detect deletions."""
    try:
        result = subprocess.run(
            ["git", "-C", str(repo), "cat-file", "-p", f"{sha}:{path}"],
            capture_output=True,
            check=False,
        )
    except FileNotFoundError as exc:
        sys.exit(f"git not on PATH: {exc}")
    if result.returncode != 0:
        return None
    return result.stdout


# ---------------------------------------------------------------------
# fact-id parsing
# ---------------------------------------------------------------------


@dataclass(frozen=True)
class FactId:
    kind: str
    # Full dotted qname as emitted by the Python labeler. For Field this
    # is `<dotted_module>.<Container>.<field>`. For
    # FunctionSignature/PublicSymbol it is `<dotted_module>.<...>.<leaf>`
    # OR (for TestAssertion) `<ClassName>.<test_fn>` / `<test_fn>`.
    qname: str
    path: str
    line: int

    @property
    def leaf(self) -> str:
        """Rightmost `.`-segment of qname — the bare symbol name as it
        appears in source (e.g. `secret_key`, `run`, `Flask`,
        `test_no_routes`)."""
        return self.qname.rsplit(".", 1)[-1]

    @property
    def container(self) -> Optional[str]:
        """For Field, the class name that owns the field — the
        penultimate `.`-segment. For other kinds, returns the
        penultimate segment IF the qname has at least two segments
        (useful as a soft class hint for methods/test classes); else
        None."""
        parts = self.qname.rsplit(".", 2)
        if len(parts) >= 2:
            return parts[-2]
        return None


def parse_fact_id(fact_id: str) -> Optional[FactId]:
    """Parse a fact_id of the form `<Kind>::<qname>::<path>::<line>`.

    Python fact_ids always have exactly four `::`-delimited segments
    (the Python labeler emits dotted qnames so the qname itself never
    contains `::`). The trailing line is always an integer.
    """
    parts = fact_id.split("::")
    if len(parts) != 4:
        return None
    kind, qname, path, line_str = parts
    if not kind or not qname or not path or not line_str:
        return None
    try:
        line = int(line_str)
    except ValueError:
        return None
    return FactId(kind=kind, qname=qname, path=path, line=line)


# ---------------------------------------------------------------------
# verdict / triage scheme (mirrors Rust autofilter)
# ---------------------------------------------------------------------


VALID = "valid"
STALE_CHANGED = "stale_source_changed"
STALE_DELETED = "stale_source_deleted"
RENAMED = "stale_symbol_renamed"
NEEDS_REVAL = "needs_revalidation"


@dataclass
class AutoVerdict:
    label: str  # one of {VALID, STALE_*, RENAMED, NEEDS_REVAL, "uncertain"}
    note: str
    confidence: str  # "high" | "medium" | "low"


# ---------------------------------------------------------------------
# source helpers
# ---------------------------------------------------------------------


def _decode(blob: bytes) -> str:
    return blob.decode("utf-8", errors="replace")


def _split_lines(text: str) -> list[str]:
    """Split keeping line indexing 1-based: index 0 is empty so that
    ``lines[N]`` is the Nth line of source."""
    return [""] + text.splitlines()


_INDENT_RE = re.compile(r"^[ \t]*")


def _indent(line: str) -> int:
    """Leading-whitespace width (tabs count as 1 — Python labelers
    operate on PEP-8 conformant flask, so tabs are essentially absent;
    we still treat both uniformly for robustness)."""
    m = _INDENT_RE.match(line)
    return len(m.group(0)) if m else 0


def _enclosing_class(lines: list[str], line_no: int) -> Optional[str]:
    """Walk backward from ``line_no - 1`` looking for the nearest
    ``class <Name>(...):`` whose indent is strictly less than the
    target line. Returns the class name or None if no such class is
    above (i.e. the target is at module scope).

    This is a heuristic — Python's grammar would require a real parser
    to be exact — but it's robust enough for PEP-8 indented sources
    like flask.
    """
    if line_no <= 0 or line_no >= len(lines):
        return None
    target_indent = _indent(lines[line_no])
    if target_indent == 0:
        return None
    class_re = re.compile(r"^(?P<ind>[ \t]*)class\s+(?P<name>[A-Za-z_]\w*)\b")
    for i in range(line_no - 1, 0, -1):
        ln = lines[i]
        if not ln.strip():
            continue
        m = class_re.match(ln)
        if m and len(m.group("ind")) < target_indent:
            return m.group("name")
    return None


def _enclosing_def(
    lines: list[str], line_no: int, prefix: str = ""
) -> Optional[str]:
    """Walk backward from ``line_no`` looking for the nearest ``def
    <name>(`` (optionally constrained to names starting with
    ``prefix``) whose indent is strictly less than the target line's
    indent. Returns the def name or None."""
    if line_no <= 0 or line_no >= len(lines):
        return None
    target_indent = _indent(lines[line_no])
    name_re = re.compile(
        r"^(?P<ind>[ \t]*)(?:async\s+)?def\s+(?P<name>"
        + (re.escape(prefix) + r"\w*" if prefix else r"[A-Za-z_]\w*")
        + r")\s*\("
    )
    for i in range(line_no, 0, -1):
        ln = lines[i]
        if not ln.strip():
            continue
        m = name_re.match(ln)
        if m and len(m.group("ind")) < target_indent:
            return m.group("name")
    return None


# ---------------------------------------------------------------------
# per-kind classifiers
# ---------------------------------------------------------------------


def classify_field(repo: Path, t0: str, sha: str, fid: FactId) -> AutoVerdict:
    """Confirm that the recorded line has a `<field>` assignment or
    annotated-assignment inside the expected class block.

    Strict-GREEN conditions:
      1. file is present at the recorded commit
      2. the recorded line, stripped, starts with `<field>` followed by
         one of: `:` (annotated assign), `=` (plain assign), `(` (tuple
         unpacking is rare; we accept), or end-of-symbol whitespace
         then `:`/`=`.
      3. the enclosing class (per indent walk-back) matches the qname's
         penultimate segment.
    """
    field = fid.leaf
    container = fid.container or ""
    post_bytes = git_cat_file(repo, sha, fid.path)
    if post_bytes is None:
        return AutoVerdict(
            STALE_DELETED, f"file `{fid.path}` absent at commit", "high"
        )
    text = _decode(post_bytes)
    lines = _split_lines(text)
    if fid.line >= len(lines):
        return AutoVerdict(
            "uncertain",
            f"line {fid.line} beyond EOF ({len(lines) - 1} lines)",
            "low",
        )
    line = lines[fid.line]
    field_pat = re.compile(
        rf"^\s*{re.escape(field)}\s*(?::|=(?!=))"
    )
    if not field_pat.match(line):
        # Allow tuple-unpacking style `field, other = ...` as a soft
        # fallback (rare in flask) — flagged YELLOW elsewhere.
        return AutoVerdict(
            STALE_DELETED,
            f"line {fid.line} in {fid.path} does not declare `{field}` "
            f"(content: {line.strip()!r})",
            "medium",
        )
    enclosing = _enclosing_class(lines, fid.line)
    if enclosing is None and container:
        # Field at module scope but qname asserts it's inside a class.
        # The Python labeler does not (currently) emit Field for
        # module-level constants, so this should never happen — flag
        # as low-confidence valid rather than disagree.
        return AutoVerdict(
            VALID,
            f"`{field}` declared at line {fid.line} but enclosing class "
            f"not detected (expected `{container}`)",
            "low",
        )
    if container and enclosing != container:
        return AutoVerdict(
            "uncertain",
            f"`{field}` declared at line {fid.line} but enclosing class "
            f"is `{enclosing}`, qname expected `{container}`",
            "low",
        )
    return AutoVerdict(
        VALID,
        f"`{container}.{field}` declared at line {fid.line} of {fid.path}",
        "high",
    )


def classify_function_signature(
    repo: Path, t0: str, sha: str, fid: FactId
) -> AutoVerdict:
    """Confirm that the recorded line is a `def <fn>(` or `async def
    <fn>(` definition.

    Strict-GREEN conditions:
      1. file present
      2. the recorded line matches `^\\s*(async\\s+)?def <leaf>\\s*\\(`
      3. enclosing-class check (if qname implies one) is consistent —
         either matches the penultimate segment, or the qname's
         penultimate segment is part of the dotted module path (module
         function rather than method) and we're at module scope.
    """
    fn = fid.leaf
    post_bytes = git_cat_file(repo, sha, fid.path)
    if post_bytes is None:
        return AutoVerdict(
            STALE_DELETED, f"file `{fid.path}` absent at commit", "high"
        )
    text = _decode(post_bytes)
    lines = _split_lines(text)
    if fid.line >= len(lines):
        return AutoVerdict(
            "uncertain",
            f"line {fid.line} beyond EOF ({len(lines) - 1} lines)",
            "low",
        )
    line = lines[fid.line]
    def_pat = re.compile(
        rf"^\s*(?:async\s+)?def\s+{re.escape(fn)}\s*\("
    )
    if not def_pat.match(line):
        # Fallback: a decorator might push the actual `def` one or two
        # lines past the recorded number (some labelers emit the
        # decorator line); peek ahead for up to two lines.
        for off in (1, 2):
            if fid.line + off < len(lines) and def_pat.match(lines[fid.line + off]):
                return AutoVerdict(
                    VALID,
                    f"`def {fn}(` at line {fid.line + off} (recorded {fid.line} "
                    f"is a decorator)",
                    "medium",
                )
        # Maybe the labeler pinned the wrong line but the function
        # genuinely exists in the file — UNCERTAIN, not DISAGREE.
        any_def = re.compile(rf"^\s*(?:async\s+)?def\s+{re.escape(fn)}\s*\(", re.M)
        if any_def.search(text):
            return AutoVerdict(
                "uncertain",
                f"`def {fn}(` exists in {fid.path} but not at line {fid.line} "
                f"(content: {line.strip()!r})",
                "low",
            )
        return AutoVerdict(
            STALE_DELETED,
            f"no `def {fn}(` in {fid.path}",
            "medium",
        )
    # Enclosing-class sanity check — only fail-soft. The qname's
    # penultimate segment may be either a class (method) or part of the
    # dotted module path (free function in a nested package).
    enclosing = _enclosing_class(lines, fid.line)
    container = fid.container
    # If qname looks like `<...>.<Cap>.<fn>` and we found that Cap as
    # the enclosing class, it's a strong-confidence method match.
    if (
        enclosing is not None
        and container is not None
        and enclosing == container
    ):
        return AutoVerdict(
            VALID,
            f"method `{container}.{fn}` defined at line {fid.line} of {fid.path}",
            "high",
        )
    # Module-scope function: enclosing class is None. That's the
    # common case for utility functions like `helpers.get_debug_flag`.
    if enclosing is None:
        return AutoVerdict(
            VALID,
            f"module-scope `def {fn}(` at line {fid.line} of {fid.path}",
            "high",
        )
    # Otherwise the def IS inside a class but the class name doesn't
    # match the qname's penultimate segment — that could be a nested
    # class or labeler quirk; surface as YELLOW.
    return AutoVerdict(
        VALID,
        f"`def {fn}(` at line {fid.line} but enclosing class `{enclosing}` "
        f"differs from qname container `{container}`",
        "low",
    )


def classify_public_symbol(
    repo: Path, t0: str, sha: str, fid: FactId
) -> AutoVerdict:
    """Confirm the recorded line declares the named symbol.

    PublicSymbol is the broadest kind — it covers ``class X``, ``def
    f``, top-level assignment ``X = ...``, and annotated assignment ``X:
    T = ...``. Strict-GREEN requires the symbol token to appear at the
    recorded line as the LHS of a declaration.
    """
    name = fid.leaf
    post_bytes = git_cat_file(repo, sha, fid.path)
    if post_bytes is None:
        return AutoVerdict(
            STALE_DELETED, f"file `{fid.path}` absent at commit", "high"
        )
    text = _decode(post_bytes)
    lines = _split_lines(text)
    if fid.line >= len(lines):
        return AutoVerdict(
            "uncertain",
            f"line {fid.line} beyond EOF ({len(lines) - 1} lines)",
            "low",
        )
    line = lines[fid.line]
    # Try each declaration pattern in turn.
    n = re.escape(name)
    class_pat = re.compile(rf"^\s*class\s+{n}\b")
    def_pat = re.compile(rf"^\s*(?:async\s+)?def\s+{n}\s*\(")
    # Top-level assignment: leading whitespace must be empty (module
    # scope). Allow type-annotated assignment too.
    assign_pat = re.compile(rf"^{n}\s*(?::|=(?!=))")
    if class_pat.match(line):
        return AutoVerdict(
            VALID,
            f"`class {name}` at line {fid.line} of {fid.path}",
            "high",
        )
    if def_pat.match(line):
        return AutoVerdict(
            VALID,
            f"`def {name}(` at line {fid.line} of {fid.path}",
            "high",
        )
    if assign_pat.match(line):
        return AutoVerdict(
            VALID,
            f"module-scope `{name} = ...` at line {fid.line} of {fid.path}",
            "high",
        )
    # Decorator immediately above the def?
    def_pat_any_indent = re.compile(
        rf"^\s*(?:async\s+)?def\s+{n}\s*\("
    )
    for off in (1, 2):
        if fid.line + off < len(lines) and def_pat_any_indent.match(
            lines[fid.line + off]
        ):
            return AutoVerdict(
                VALID,
                f"`def {name}(` at line {fid.line + off} "
                f"(recorded {fid.line} is a decorator)",
                "medium",
            )
    # Symbol exists somewhere in the file?
    any_decl = re.compile(
        rf"^\s*(?:class\s+{n}\b|(?:async\s+)?def\s+{n}\s*\(|{n}\s*[:=])",
        re.M,
    )
    if any_decl.search(text):
        return AutoVerdict(
            "uncertain",
            f"`{name}` declared in {fid.path} but not at line {fid.line} "
            f"(content: {line.strip()!r})",
            "low",
        )
    return AutoVerdict(
        STALE_DELETED,
        f"no declaration of `{name}` in {fid.path}",
        "medium",
    )


_ASSERT_RE = re.compile(
    r"\b(?:assert\b|self\.assert\w+\s*\(|pytest\.(?:raises|warns|fail|approx|deprecated_call)\b|"
    r"with\s+pytest\.(?:raises|warns|deprecated_call)\b)"
)


def classify_test_assertion(
    repo: Path, t0: str, sha: str, fid: FactId
) -> AutoVerdict:
    """Confirm that the recorded line is inside a `def test_*` function
    and contains an assertion construct.

    Strict-GREEN conditions:
      1. file present
      2. recorded line contains an `assert` keyword, a `self.assertX(`
         call, or a `pytest.raises`/`warns`/`approx`/etc. construct.
      3. the enclosing `def` (per indent walk-back) starts with `test_`
         AND its name matches the qname's leaf.
    """
    fn = fid.leaf
    post_bytes = git_cat_file(repo, sha, fid.path)
    if post_bytes is None:
        return AutoVerdict(
            STALE_DELETED, f"file `{fid.path}` absent at commit", "high"
        )
    text = _decode(post_bytes)
    lines = _split_lines(text)
    if fid.line >= len(lines):
        return AutoVerdict(
            "uncertain",
            f"line {fid.line} beyond EOF ({len(lines) - 1} lines)",
            "low",
        )
    line = lines[fid.line]
    has_assertion = _ASSERT_RE.search(line) is not None
    enclosing_fn = _enclosing_def(lines, fid.line)
    if enclosing_fn is None:
        return AutoVerdict(
            "uncertain",
            f"line {fid.line} of {fid.path} is not inside any `def` "
            f"(content: {line.strip()!r})",
            "low",
        )
    if not enclosing_fn.startswith("test_") and not fn.startswith("test_"):
        # Some flask helpers have non-`test_`-prefixed fns that the
        # labeler may have classified as TestAssertion — escalate.
        return AutoVerdict(
            "uncertain",
            f"enclosing fn `{enclosing_fn}` is not a `test_*` function",
            "low",
        )
    if enclosing_fn != fn:
        return AutoVerdict(
            "uncertain",
            f"line {fid.line} is inside `{enclosing_fn}` but qname leaf is `{fn}`",
            "low",
        )
    if not has_assertion:
        # Line is inside the right test fn but doesn't textually look
        # like an assertion. Could be a comment or setup line that the
        # labeler tied to the fact for context. We surface YELLOW
        # rather than DISAGREE — the labeler's notion of "assertion"
        # may include indirect helpers (e.g. `client.post(...)`
        # followed by a chained `.assert_*`).
        return AutoVerdict(
            VALID,
            f"line {fid.line} inside test fn `{fn}` but no `assert`/`pytest.raises` "
            f"detected on that line (content: {line.strip()!r})",
            "low",
        )
    return AutoVerdict(
        VALID,
        f"assertion at line {fid.line} inside `{fn}` of {fid.path}",
        "high",
    )


CLASSIFIERS = {
    "PublicSymbol": classify_public_symbol,
    "FunctionSignature": classify_function_signature,
    "Field": classify_field,
    "TestAssertion": classify_test_assertion,
}


# ---------------------------------------------------------------------
# triage
# ---------------------------------------------------------------------


def triage(predicted: str, verdict: AutoVerdict) -> tuple[str, str]:
    """Return (auto_tag, auto_note)."""
    note = f"{verdict.label} ({verdict.confidence}): {verdict.note}"
    if verdict.label == "uncertain":
        return "UNCERTAIN", note
    if verdict.label == predicted:
        if verdict.confidence == "high":
            return "GREEN", note
        return "YELLOW", note
    # Auto-derived label distinguishes renamed/needs_reval from deleted
    # only weakly; escalate rather than disagree outright in those
    # cases.
    if predicted in {RENAMED, NEEDS_REVAL} and verdict.label == STALE_DELETED:
        return "YELLOW", (
            f"auto says deleted; labeler says {predicted}; "
            f"rename/reval check exceeds auto-filter scope: {verdict.note}"
        )
    return "DISAGREE", note


# ---------------------------------------------------------------------
# CLI
# ---------------------------------------------------------------------


def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument("--csv", required=True, type=Path)
    p.add_argument("--repo", required=True, type=Path)
    p.add_argument("--t0", default=T0_DEFAULT)
    p.add_argument("--out", required=True, type=Path)
    args = p.parse_args()

    with args.csv.open(newline="") as f:
        reader = csv.DictReader(f)
        rows = list(reader)
        fieldnames = list(reader.fieldnames or [])

    out_fieldnames = fieldnames + ["auto_tag", "auto_note"]
    counts: dict[str, int] = {
        "GREEN": 0,
        "YELLOW": 0,
        "DISAGREE": 0,
        "UNCERTAIN": 0,
        "PARSE_ERROR": 0,
    }
    per_kind: dict[str, dict[str, int]] = {}

    enriched = []
    for row in rows:
        fid = parse_fact_id(row["fact_id"])
        if fid is None:
            row["auto_tag"] = "PARSE_ERROR"
            row["auto_note"] = "could not parse fact_id"
            counts["PARSE_ERROR"] += 1
            enriched.append(row)
            continue
        classifier = CLASSIFIERS.get(fid.kind)
        if classifier is None:
            row["auto_tag"] = "PARSE_ERROR"
            row["auto_note"] = f"unknown fact kind `{fid.kind}`"
            counts["PARSE_ERROR"] += 1
            enriched.append(row)
            continue
        verdict = classifier(args.repo, args.t0, row["commit_sha"], fid)
        tag, note = triage(row["predicted_label"], verdict)
        row["auto_tag"] = tag
        row["auto_note"] = note
        counts[tag] = counts.get(tag, 0) + 1
        per_kind.setdefault(fid.kind, {})[tag] = (
            per_kind.setdefault(fid.kind, {}).get(tag, 0) + 1
        )
        enriched.append(row)

    with args.out.open("w", newline="") as f:
        writer = csv.DictWriter(f, fieldnames=out_fieldnames)
        writer.writeheader()
        for row in enriched:
            writer.writerow(row)

    total = sum(counts.values())
    print(f"wrote {total} rows to {args.out}")
    for tag, n in sorted(counts.items(), key=lambda kv: -kv[1]):
        if n:
            print(f"  {tag:11s} {n:4d}  ({n / total:5.1%})")
    print()
    print("Per kind:")
    kind_order = ["Field", "FunctionSignature", "PublicSymbol", "TestAssertion"]
    for kind in kind_order:
        d = per_kind.get(kind, {})
        if not d:
            continue
        kt = sum(d.values())
        bits = ", ".join(
            f"{t}={d[t]}" for t in ("GREEN", "YELLOW", "DISAGREE", "UNCERTAIN") if d.get(t)
        )
        print(f"  {kind:18s} (n={kt:3d}): {bits}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
