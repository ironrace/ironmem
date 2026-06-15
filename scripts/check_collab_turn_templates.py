#!/usr/bin/env python3
"""Lint for collab worker-per-turn templates and the collab.md dispatch surface.

Exit 0 iff all checks pass; non-zero with a printed reason otherwise.
Stdlib only.
"""
from __future__ import annotations
import re, sys, pathlib

ROOT = pathlib.Path(__file__).resolve().parents[1]
PROMPTS = ROOT / ".claude-plugin" / "prompts"
COMMAND = ROOT / ".claude-plugin" / "commands" / "collab.md"

ALLOWED_PLACEHOLDERS = {"SESSION_ID", "REPO_PATH", "BRANCH", "TOPIC",
                        "ARTIFACT_REF", "MODE"}
REQUIRED_FM = {"turn", "tier", "model", "topics", "preconditions"}
VALID_TIERS = {"planning", "review", "mechanical"}
VALID_MODELS = {"opus", "sonnet", "haiku", "default"}
VALID_TOPICS = {"draft", "canonical", "review", "final", "task_list",
                "implementation_done", "review_local", "review_fix_global",
                "final_review", "failure_report"}
REQUIRED_SENTINELS = ["<!-- LINT:worker-dispatch -->",
                      "<!-- LINT:gates-ref-only -->",
                      "<!-- LINT:bridge-worker-owned -->",
                      "<!-- LINT:fail-closed-tiering -->",
                      "<!-- LINT:dispatch-matrix -->"]
# Legacy inline-orchestrator instructions that must NOT survive the rewrite.
FORBIDDEN_IN_COMMAND = [
    "Derive the `task_list` manifest from the markdown",
]
PLACEHOLDER_RE = re.compile(r"\$([A-Za-z_]+)")
FM_RE = re.compile(r"^---\n(.*?)\n---\n", re.DOTALL)

errors: list[str] = []


def err(msg: str) -> None:
    errors.append(msg)


def parse_frontmatter(text: str) -> dict | None:
    m = FM_RE.match(text)
    if not m:
        return None
    fm: dict = {}
    for line in m.group(1).splitlines():
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if ":" not in line:
            continue
        k, _, v = line.partition(":")
        fm[k.strip()] = v.strip()
    return fm


def parse_topics(raw: str) -> list[str]:
    raw = raw.strip().strip("[]")
    return [t.strip().strip("'\"") for t in raw.split(",") if t.strip()]


def lint_template(path: pathlib.Path) -> dict | None:
    text = path.read_text()
    name = path.name
    fm = parse_frontmatter(text)
    if fm is None:
        err(f"{name}: missing YAML frontmatter")
        return None
    missing = REQUIRED_FM - set(fm)
    if missing:
        err(f"{name}: missing frontmatter keys: {sorted(missing)}")
    if fm.get("tier") not in VALID_TIERS:
        err(f"{name}: invalid tier {fm.get('tier')!r}")
    if fm.get("model") not in VALID_MODELS:
        err(f"{name}: invalid model {fm.get('model')!r}")
    topics = parse_topics(fm.get("topics", ""))
    bad_topics = [t for t in topics if t not in VALID_TOPICS]
    if bad_topics:
        err(f"{name}: invalid topics {bad_topics}")
    body = text[FM_RE.match(text).end():] if FM_RE.match(text) else text
    for ph in set(PLACEHOLDER_RE.findall(body)):
        if ph.isupper() and ph not in ALLOWED_PLACEHOLDERS:
            err(f"{name}: unknown placeholder ${ph}")
    if "ANTI-PUPPETEERING" not in body:
        err(f"{name}: missing ANTI-PUPPETEERING banner")
    if "## Verdict" not in body:
        err(f"{name}: missing '## Verdict' (<=3 line contract) section")
    if re.search(r"fable", text, re.IGNORECASE):
        err(f"{name}: contains a 'Fable' reference (Fable is OFF)")
    return {"name": name, "tier": fm.get("tier"), "model": fm.get("model"),
            "topics": topics}


def parse_dispatch_matrix(text: str) -> list[dict]:
    """Rows of a markdown table whose Template cell names a collab-turn-*.md."""
    rows = []
    for line in text.splitlines():
        if "collab-turn-" not in line or "|" not in line:
            continue
        cells = [c.strip().strip("`") for c in line.strip().strip("|").split("|")]
        tmpl = next((c for c in cells if c.endswith(".md") and
                     c.startswith("collab-turn-")), None)
        if not tmpl:
            continue
        tier = next((c for c in cells if c in VALID_TIERS), None)
        model = next((c for c in cells if c in VALID_MODELS), None)
        rows.append({"template": tmpl, "tier": tier, "model": model})
    return rows


def main() -> int:
    if not PROMPTS.is_dir():
        err(f"prompts dir missing: {PROMPTS}")
        print("\n".join(errors)); return 1
    templates = sorted(PROMPTS.glob("collab-turn-*.md"))
    if not templates:
        err("no collab-turn-*.md templates found")
    parsed = {}
    for t in templates:
        info = lint_template(t)
        if info:
            parsed[info["name"]] = info

    cmd_text = COMMAND.read_text()
    for s in REQUIRED_SENTINELS:
        if s not in cmd_text:
            err(f"collab.md: missing sentinel {s}")
    for f in FORBIDDEN_IN_COMMAND:
        if f in cmd_text:
            err(f"collab.md: forbidden legacy instruction present: {f!r}")
    # Fable allowed in collab.md only on an explicit OFF/disabled line.
    for i, line in enumerate(cmd_text.splitlines(), 1):
        if re.search(r"fable", line, re.IGNORECASE) and not re.search(
                r"off|disabled|do not|never", line, re.IGNORECASE):
            err(f"collab.md:{i}: 'Fable' reference without OFF/disabled context")
    # Bridge boundary: bridge section must name the task-list worker.
    if "collab-turn-task-list.md" not in cmd_text:
        err("collab.md: bridge must reference collab-turn-task-list.md worker")

    # Matrix <-> frontmatter cross-check.
    matrix = parse_dispatch_matrix(cmd_text)
    matrix_tmpls = {r["template"] for r in matrix}
    for r in matrix:
        info = parsed.get(r["template"])
        if not info:
            err(f"matrix references missing template {r['template']}")
            continue
        if r["tier"] and r["tier"] != info["tier"]:
            err(f"{r['template']}: matrix tier {r['tier']} != frontmatter "
                f"{info['tier']}")
        if r["model"] and r["model"] != info["model"]:
            err(f"{r['template']}: matrix model {r['model']} != frontmatter "
                f"{info['model']}")
    for name in parsed:
        if name not in matrix_tmpls:
            err(f"{name}: no dispatch-matrix row in collab.md")

    if errors:
        print("collab-turn template lint FAILED:")
        for e in errors:
            print(f"  - {e}")
        return 1
    print(f"collab-turn template lint OK ({len(parsed)} templates, "
          f"{len(matrix)} matrix rows)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
