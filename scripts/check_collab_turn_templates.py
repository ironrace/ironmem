#!/usr/bin/env python3
"""Lint collab worker templates and Codex's phase-prompt dispatch surface.

Exit 0 iff all checks pass; non-zero with a printed reason otherwise.
Stdlib only.
"""
from __future__ import annotations
import os, pathlib, re, sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))
from sync_skills import UNINSTALLED_SKILL_NAMES  # noqa: E402

ROOT = pathlib.Path(os.environ.get(
    "COLLAB_LINT_ROOT",
    pathlib.Path(__file__).resolve().parents[1],
)).resolve()
PROMPTS = ROOT / ".claude-plugin" / "prompts"
COMMAND = ROOT / ".claude-plugin" / "commands" / "collab.md"
DOC = ROOT / "docs" / "COLLAB.md"
EVALUATE_ISSUE_DOC = ROOT / "docs" / "EVALUATE_ISSUE.md"
EVALUATE_ISSUE_CLAUDE = ROOT / ".claude-plugin" / "commands" / "evaluate-issue.md"
EVALUATE_ISSUE_CODEX = ROOT / ".codex-plugin" / "prompts" / "evaluate-issue.md"
CODEX_COMMAND = ROOT / ".codex-plugin" / "commands" / "collab.md"
CODEX_PROMPTS = [
    ROOT / ".codex-plugin" / "prompts" / name
    for name in (
        "collab-plan-draft.md",
        "collab-plan-synthesis.md",
        "collab-plan-review.md",
        "collab-plan-finalize.md",
        "collab-task-list.md",
        "collab-batch-impl.md",
        "collab-global-review.md",
        "collab-review-local.md",
        "collab-final-review.md",
        "collab-recovery.md",
    )
]
REVIEW_DIFF_FALLBACK_SURFACES = {
    ROOT / ".codex-plugin" / "prompts" / "collab-global-review.md": [
        "ironmem review-diff --repo <repo_path> --base <base_sha> --head <last_head_sha>",
        "only on success",
        "git diff <base_sha>..<last_head_sha>",
        "--expand-file <path> --hunk <ordinal>",
    ],
    ROOT / ".codex-plugin" / "prompts" / "collab-review-local.md": [
        "ironmem review-diff --repo <repo_path> --base <base_sha> --head <last_head_sha>",
        "only on success",
        "git diff <base_sha>..<last_head_sha>",
        "--expand-file <path> --hunk <ordinal>",
    ],
    PROMPTS / "collab-turn-review-local.md": [
        "ironmem review-diff --repo <repo_path> --base <base_sha> --head <last_head_sha>",
        "only on success",
        "git diff <base_sha>..<last_head_sha>",
        "--expand-file <path> --hunk <ordinal>",
    ],
    PROMPTS / "collab-turn-review-fix-global.md": [
        "ironmem review-diff --repo <repo_path> --base <base_sha> --head <last_head_sha>",
        "only on success",
        "git diff <base_sha>..<last_head_sha>",
        "--expand-file <path> --hunk <ordinal>",
    ],
    ROOT / ".claude-plugin" / "commands" / "ultrareview-local.md": [
        "ironmem review-diff --repo <repo_path> --base <baseRefName> --head <headRefName>",
        "ironmem review-diff --repo <repo_path> --worktree",
        "only on success",
        "gh pr diff <N>",
        "git diff HEAD",
        "--expand-file <path> --hunk <ordinal>",
    ],
}
REVIEW_DIFF_TRIGGER_DETECTION_SNIPPETS = [
    "Preserve the full raw diff transiently for deterministic trigger detection",
    "Do not inject or repeat that raw diff in reviewer prompts",
    "must not treat the lossy artifact as the\nsole classifier",
]

ALLOWED_PLACEHOLDERS = {"SESSION_ID", "REPO_PATH", "BRANCH", "TOPIC",
                        "ARTIFACT_REF", "ARTIFACT_HASH", "MODE"}
REQUIRED_FM = {"turn", "tier", "model", "topics", "preconditions"}
VALID_TIERS = {"planning", "review", "mechanical"}
VALID_MODELS = {"opus", "sonnet", "haiku", "default"}
VALID_TOPICS = {"draft", "canonical", "review", "final", "task_list",
                "implementation_done", "review_local", "review_fix_global",
                "final_review", "failure_report"}
EXPECTED_TEMPLATES = {
    "collab-turn-plan-draft.md": {
        "turn": "draft",
        "tier": "planning",
        "model": "opus",
        "topics": ["draft"],
    },
    "collab-turn-plan-synthesis.md": {
        "turn": "canonical",
        "tier": "planning",
        "model": "opus",
        "topics": ["canonical"],
    },
    "collab-turn-plan-review.md": {
        "turn": "review",
        "tier": "review",
        "model": "opus",
        "topics": ["review"],
    },
    "collab-turn-plan-finalize.md": {
        "turn": "final",
        "tier": "planning",
        "model": "opus",
        "topics": ["final"],
    },
    "collab-turn-task-list.md": {
        "turn": "task_list",
        "tier": "mechanical",
        "model": "sonnet",
        "topics": ["task_list"],
    },
    "collab-turn-code-implement.md": {
        "turn": "implementation_done",
        "tier": "mechanical",
        "model": "sonnet",
        "topics": ["implementation_done", "failure_report"],
    },
    "collab-turn-review-fix-global.md": {
        "turn": "review_fix_global",
        "tier": "review",
        "model": "opus",
        "topics": ["review_fix_global", "failure_report"],
    },
    "collab-turn-review-local.md": {
        "turn": "review_local",
        "tier": "review",
        "model": "opus",
        "topics": ["review_local", "failure_report"],
    },
    "collab-turn-final-review.md": {
        "turn": "final_review",
        "tier": "review",
        "model": "opus",
        "topics": ["final_review"],
    },
    "collab-turn-submit.md": {
        "turn": "submit",
        "tier": "mechanical",
        "model": "sonnet",
        "topics": ["final", "final_review", "failure_report"],
    },
}
REQUIRED_TEMPLATE_SNIPPETS = {
    "collab-turn-plan-synthesis.md": [
        "first auto-ack response",
        "get_drawer(id=<message.drawer_id>)",
        "Do not call `collab_recv` again after it acknowledges.",
        "`full:true` is compatibility-only",
    ],
    "collab-turn-task-list.md": [
        "Timebox: <=20 minutes",
        "more than 10 tasks",
        "PlanLocked is pre-coding",
        "plan_file_path",
    ],
    "collab-turn-plan-finalize.md": [
        "Timebox: <=20 minutes",
        "at most 10 tasks",
        "docs/iron/plans/YYYY-MM-DD-<short-feature>.md",
        "first auto-ack response",
        "get_drawer(id=<canonical_plan_ref.drawer_id>)",
        "get_drawer(id=<message.drawer_id>)",
        "Do not call `collab_recv` again after it acknowledges.",
        "`full:true` is compatibility-only",
    ],
    "collab-turn-final-review.md": [
        '{"title":"<title>","body":"<body>"}',
        "do NOT re-run gates",
        "pushed-head proof",
    ],
    "collab-turn-submit.md": [
        'parse the artifact JSON as',
        'gh pr create --base <base_branch>',
    ],
}
FORBIDDEN_TEMPLATE_SNIPPETS = {
    "collab-turn-plan-synthesis.md": [
        "read Codex's draft",
    ],
    "collab-turn-plan-finalize.md": [
        "read `canonical_plan`",
        "read Codex's review notes",
    ],
}
CHECKPOINT_PROTOCOL_SURFACES = {
    DOC: "docs/COLLAB.md",
    COMMAND: ".claude-plugin/commands/collab.md",
    PROMPTS / "collab-turn-code-implement.md": ".claude-plugin/prompts/collab-turn-code-implement.md",
    ROOT / ".codex-plugin" / "prompts" / "collab-batch-impl.md": ".codex-plugin/prompts/collab-batch-impl.md",
}
REQUIRED_CHECKPOINT_PROTOCOL_SNIPPETS = [
    "collab-checkpoint:<session_id>",
    "one logical-keyed current drawer",
    "get_drawer(wing=ironrace-memory",
    "completed_task_ids",
]
REQUIRED_SENTINELS = ["<!-- LINT:worker-dispatch -->",
                      "<!-- LINT:gates-ref-only -->",
                      "<!-- LINT:bridge-worker-owned -->",
                      "<!-- LINT:fail-closed-tiering -->",
                      "<!-- LINT:dispatch-matrix -->"]
REQUIRED_EVALUATE_ISSUE_SNIPPETS = [
    "Verdict: <DIRECT | IRON | COLLAB | SPLIT>",
    "Task estimate: <N | N+> independent execution tasks",
    "more than 10",
    "Child issues:",
    "Parent: #<number>",
    "advisory-only",
    "Split-child-key:",
    "Split-parent-key:",
]
# Legacy inline-orchestrator instructions that must NOT survive the rewrite.
FORBIDDEN_IN_COMMAND = [
    "Derive the `task_list` manifest from the markdown",
]
PLACEHOLDER_RE = re.compile(r"\$([A-Za-z_]+)")
FM_RE = re.compile(r"^---\n(.*?)\n---\n", re.DOTALL)
VERDICT_RE = re.compile(r"## Verdict.*?```(?:[a-zA-Z0-9_-]+)?\n(.*?)\n```",
                        re.DOTALL)

errors: list[str] = []

# ---- coding_failure prefix cross-check -------------------------------------
#
# The prompt surfaces tell agents which `coding_failure` strings to send, and
# the server decides — from `crates/ironmem/src/collab/mod.rs` — which ones it
# will admit off-turn and which classify as recoverable. Nothing used to
# connect the two, and the drift was real: `.claude-plugin/commands/collab.md`
# instructed the dispatcher to send `codex_exec_failed_silent:`,
# `codex_exec_timeout` and `codex_exec_env_error:`, none of which exist
# anywhere in the Rust. A Codex process dying silently produced a report the
# server rejected as "not your turn", leaving the session stuck mid-phase with
# no failure recorded at all.
#
# These two checks close that class mechanically instead of by review.
COLLAB_RS = ROOT / "crates" / "ironmem" / "src" / "collab" / "mod.rs"
RUST_PREFIX_RE = re.compile(
    r'pub const [A-Z0-9_]+_PREFIX: &str = "([a-z0-9_]+:)";')
# Prefixes the surfaces may legitimately use that are deliberately absent from
# the Rust constants: `classify()` maps every unrecognized string to
# `FailureClass::Terminal`, and these two are documented terminal failures. Any
# OTHER unrecognized prefix is far more likely a typo than a deliberate
# terminal report, so it fails the gate and has to be added here on purpose.
#
# Every entry here is reported by the phase's own on-turn owner, so off-turn
# admissibility never applies to it, and every one is meant to stop the
# session for human attention rather than hand off a retry:
#   subagent_failure:              a task the batch could not complete
#   gate_failure:                  fmt/clippy/test gate red at the end of a batch
#   mechanical_direct_gate_failed: the same, on Codex's mechanical_direct path
#   skill_overran_pr_boundary:     a sub-skill opened a PR outside the protocol
#   pr_create_failed:              `gh pr create` failed on Claude's final turn
# `pr_create_failed:` is deliberately NOT the prefix Codex uses when it owns
# `final_review` under recovery — there it reports `network_failed:` /
# `sandbox_denied:`, which classify Tooling and hand the PR turn to Claude,
# who owns PR creation in the normal flow. The asymmetry is intentional: a
# Codex PR failure has a live counterpart to fall back on, Claude's does not.
DOCUMENTED_TERMINAL_PREFIXES = {
    "subagent_failure:",
    "gate_failure:",
    "mechanical_direct_gate_failed:",
    "skill_overran_pr_boundary:",
    "pr_create_failed:",
}
# `coding_failure` in the surfaces is followed by the value across a few
# punctuation shapes: `coding_failure: "git_push_failed: …"`,
# `{"coding_failure":"branch_drift: …"}`, `coding_failure = 'disk_full: …'`.
# Allow a short run of non-word characters between the key and the prefix, and
# capture the prefix token up to its colon.
CODING_FAILURE_USE_RE = re.compile(r'coding_failure\W{0,8}([a-z][a-z0-9_]*):')


def failure_prefix_surfaces() -> list[pathlib.Path]:
    """Every markdown surface that may instruct an agent to send a failure."""
    paths = [COMMAND, DOC, CODEX_COMMAND, *CODEX_PROMPTS]
    for directory in (PROMPTS, ROOT / ".codex-plugin" / "prompts"):
        if directory.is_dir():
            paths.extend(sorted(directory.glob("*.md")))
    # Deduplicate while preserving order: phase prompts are also in the
    # .codex-plugin prompts directory.
    seen: set[pathlib.Path] = set()
    unique = []
    for path in paths:
        if path.exists() and path not in seen:
            seen.add(path)
            unique.append(path)
    return unique


def check_failure_prefixes() -> None:
    if not COLLAB_RS.exists():
        err(f"{COLLAB_RS}: cannot cross-check coding_failure prefixes — "
            f"collab/mod.rs not found")
        return
    rust_text = COLLAB_RS.read_text()
    rust_prefixes = set(RUST_PREFIX_RE.findall(rust_text))
    if not rust_prefixes:
        err("collab/mod.rs: no *_PREFIX constants parsed — the prefix "
            "cross-check would pass vacuously, so it fails instead")
        return
    known = rust_prefixes | DOCUMENTED_TERMINAL_PREFIXES

    for path in failure_prefix_surfaces():
        rel = path.relative_to(ROOT)
        for line_no, line in enumerate(path.read_text().splitlines(), 1):
            for token in CODING_FAILURE_USE_RE.findall(line):
                prefix = f"{token}:"
                if prefix not in known:
                    err(f"{rel}:{line_no}: coding_failure prefix {prefix!r} "
                        f"is not declared in collab/mod.rs and is not a "
                        f"documented terminal prefix — the server would "
                        f"reject or misclassify it")

    # Drift in the other direction: a prefix the server recognizes but that
    # docs/COLLAB.md never mentions is a prefix no agent will ever be told to
    # send.
    doc_body = DOC.read_text()
    for prefix in sorted(rust_prefixes):
        if prefix not in doc_body:
            err(f"docs/COLLAB.md: recognized failure prefix {prefix!r} is "
                f"never documented")


def check_no_uninstalled_skill_references() -> None:
    """No instruction surface may name a skill this repo does not install.

    test_sync_skills.py enforces the same denylist over the generated `skills/`
    tree; that gate stops at the skill boundary, so a stale name reintroduced
    into a prompt, the command file, or docs/COLLAB.md would otherwise ship
    green. The skill resolves to nothing on a standalone install (the names are
    in install-ironmem.sh's LEGACY_SHARED_SKILLS and are deleted on upgrade),
    and the worker proceeds with its own approach with no protocol-level signal.
    """
    for path in failure_prefix_surfaces():
        rel = path.relative_to(ROOT)
        for line_no, line in enumerate(path.read_text().splitlines(), 1):
            lowered = line.lower()
            for name in UNINSTALLED_SKILL_NAMES:
                if name in lowered:
                    err(f"{rel}:{line_no}: names uninstalled skill {name!r} — "
                        f"it is removed on install, so the instruction resolves "
                        f"to nothing and the worker freelances the step")


def check_evaluate_issue_surfaces() -> None:
    """Keep the evaluator's SPLIT safety contract in its three mirrors."""
    for path in (EVALUATE_ISSUE_DOC, EVALUATE_ISSUE_CLAUDE, EVALUATE_ISSUE_CODEX):
        if not path.exists():
            err(f"{path.relative_to(ROOT)}: missing evaluate-issue surface")
            continue
        body = path.read_text()
        for snippet in REQUIRED_EVALUATE_ISSUE_SNIPPETS:
            if snippet not in body:
                err(f"{path.relative_to(ROOT)}: missing evaluate-issue SPLIT contract "
                    f"snippet {snippet!r}")


def check_review_diff_fallback_contract() -> None:
    """Keep every review entrypoint artifact-first with a raw fallback."""
    for path, snippets in REVIEW_DIFF_FALLBACK_SURFACES.items():
        if not path.exists() or any(snippet not in path.read_text() for snippet in snippets):
            err(f"{path.relative_to(ROOT)}: missing review-diff fallback contract")


def check_review_diff_trigger_detection_contract() -> None:
    """Conditional reviewer selection needs raw source, never lossy summaries."""
    path = ROOT / ".claude-plugin" / "commands" / "ultrareview-local.md"
    if not path.exists():
        err(f"{path.relative_to(ROOT)}: missing review-diff trigger-detection contract")
        return
    text = path.read_text()
    # PR and worktree modes each need the raw-detection-only boundary.
    if any(text.count(snippet) < 2 for snippet in REVIEW_DIFF_TRIGGER_DETECTION_SNIPPETS):
        err(f"{path.relative_to(ROOT)}: missing review-diff trigger-detection contract")


def check_pr_base_resolution_contract() -> None:
    """The PR base is the integration branch, never gated on `base_sha`.

    Regression guard for a live session that died at the final turn: the base
    branch was resolved by requiring `base_sha` to be *contained* in
    `origin/main`. A collab branch is routinely cut from a local commit that
    never landed on the remote default, so containment failed, resolution
    failed, and the turn reported `pr_create_failed:` — which classifies
    Terminal. A whole reviewed, pushed, green branch lost its session at the
    one step that only opens a PR.

    Containment may inform the body (it marks commits that predate the
    reviewed range and so were reviewed by nobody); it must never decide
    whether the turn can proceed.
    """
    path = PROMPTS / "collab-turn-submit.md"
    if not path.exists():
        err(f"{path.relative_to(ROOT)}: missing submit template")
        return
    text = path.read_text()
    for snippet in (
        "integration branch",
        "never a requirement",
        "MUST NOT fail this turn",
        "Unreviewed commits in this PR",
    ):
        if snippet not in text:
            err(f"{path.relative_to(ROOT)}: missing PR-base resolution "
                f"contract {snippet!r}")
    # The exact shape of the original defect: resolution conditioned on the
    # base branch containing base_sha.
    if re.search(r"when they contain that commit", text):
        err(f"{path.relative_to(ROOT)}: base-branch resolution must not be "
            f"gated on base_sha containment")


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
    else:
        verdict = VERDICT_RE.search(body)
        if not verdict:
            err(f"{name}: missing fenced verdict block")
        else:
            lines = [line.strip() for line in verdict.group(1).splitlines()
                     if line.strip()]
            expected_prefixes = ["result:", "ref:", "blocker:"]
            if len(lines) != 3:
                err(f"{name}: verdict block must contain exactly 3 lines")
            elif [line.split(":", 1)[0] + ":" for line in lines] != expected_prefixes:
                err(f"{name}: verdict block must be result/ref/blocker lines")
    if re.search(r"fable", text, re.IGNORECASE):
        err(f"{name}: contains a 'Fable' reference (Fable is OFF)")
    for snippet in REQUIRED_TEMPLATE_SNIPPETS.get(name, []):
        if snippet not in text:
            err(f"{name}: missing required contract snippet {snippet!r}")
    for stale_claim in FORBIDDEN_TEMPLATE_SNIPPETS.get(name, []):
        if stale_claim in text:
            err(f"{name}: forbidden stale direct-body claim {stale_claim!r}")
    return {"name": name, "turn": fm.get("turn"), "tier": fm.get("tier"),
            "model": fm.get("model"), "topics": topics}


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
    template_names = {t.name for t in templates}
    expected_names = set(EXPECTED_TEMPLATES)
    for missing in sorted(expected_names - template_names):
        err(f"missing required template {missing}")
    for extra in sorted(template_names - expected_names):
        err(f"unexpected collab-turn template {extra}")
    parsed = {}
    for t in templates:
        info = lint_template(t)
        if info:
            parsed[info["name"]] = info
            expected = EXPECTED_TEMPLATES.get(info["name"])
            if expected:
                for key in ("turn", "tier", "model"):
                    if info[key] != expected[key]:
                        err(f"{info['name']}: {key} {info[key]!r} != "
                            f"expected {expected[key]!r}")
                if info["topics"] != expected["topics"]:
                    err(f"{info['name']}: topics {info['topics']} != "
                        f"expected {expected['topics']}")

    cmd_text = COMMAND.read_text()
    for s in REQUIRED_SENTINELS:
        if s not in cmd_text:
            err(f"collab.md: missing sentinel {s}")
    for f in FORBIDDEN_IN_COMMAND:
        if f in cmd_text:
            err(f"collab.md: forbidden legacy instruction present: {f!r}")
    for snippet in [
        "Skip all local gates when `phase == CodeReviewFinalPending`",
        "pushed-head proof only (no reset, no gate rerun)",
    ]:
        if snippet not in cmd_text:
            err(f"collab.md: missing final-review gate-skip contract {snippet!r}")
    if "re-runs gates" in cmd_text:
        err("collab.md: CodeReviewFinalPending must not re-run gates")
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
    if matrix_tmpls != expected_names:
        err("collab.md: dispatch matrix must reference exactly the 8 "
            "required collab-turn templates")
    for r in matrix:
        info = parsed.get(r["template"])
        if not info:
            err(f"matrix references missing template {r['template']}")
            continue
        # A matrix row naming a collab-turn template MUST carry a recognized
        # tier and model token. A typo (e.g. `mechnical`) parses to None; do
        # not silently skip the cross-check — that hides the typo.
        if r["tier"] is None or r["model"] is None:
            err(f"matrix row for {r['template']}: unrecognized tier/model "
                f"token")
            continue
        if r["tier"] != info["tier"]:
            err(f"{r['template']}: matrix tier {r['tier']} != frontmatter "
                f"{info['tier']}")
        if r["model"] != info["model"]:
            err(f"{r['template']}: matrix model {r['model']} != frontmatter "
                f"{info['model']}")
    for name in parsed:
        if name not in matrix_tmpls:
            err(f"{name}: no dispatch-matrix row in collab.md")

    doc_text = DOC.read_text()
    if "## Worker-per-turn dispatch (Claude side)" not in doc_text:
        err("docs/COLLAB.md: missing Worker-per-turn dispatch section")
    if "### Measurement gate" not in doc_text:
        err("docs/COLLAB.md: missing worker context measurement gate")
    if "Pushed-head proof" not in doc_text:
        err("docs/COLLAB.md: missing final-review pushed-head proof section")
    for name in EXPECTED_TEMPLATES:
        if name not in doc_text:
            err(f"docs/COLLAB.md: missing template reference {name}")

    for path, label in CHECKPOINT_PROTOCOL_SURFACES.items():
        if not path.exists():
            err(f"{label}: missing checkpoint protocol surface")
            continue
        text = path.read_text()
        for snippet in REQUIRED_CHECKPOINT_PROTOCOL_SNIPPETS:
            if snippet not in text:
                err(f"{label}: missing checkpoint contract {snippet!r}")

    for prompt in CODEX_PROMPTS:
        if not prompt.exists():
            err(f"{prompt.relative_to(ROOT)}: missing Codex phase prompt")
            continue
        codex_text = prompt.read_text()
        if codex_text.count("$ARGUMENTS") != 1:
            err(f"{prompt.relative_to(ROOT)}: must contain exactly one $ARGUMENTS")
        else:
            prefix = codex_text.split("$ARGUMENTS", 1)[0]
            if prefix.rstrip().splitlines()[-1] != "## Invocation":
                err(f"{prompt.relative_to(ROOT)}: $ARGUMENTS must be in final Invocation section")
            if "## Invocation" not in prefix or prefix.rfind("## Invocation") < prefix.rfind("\n## "):
                err(f"{prompt.relative_to(ROOT)}: missing final ## Invocation section")

    if not CODEX_COMMAND.exists():
        err(".codex-plugin/commands/collab.md: missing Codex slash command")
    else:
        codex_cmd_text = CODEX_COMMAND.read_text()
        for snippet in [
            "$ARGUMENTS",
            "collab-plan-draft.md",
            "collab-plan-review.md",
            "collab-global-review.md",
            "collab-recovery.md",
            "collab-batch-impl.md",
            "mcp__ironmem__collab_*",
            "one invocation handles one",
            "collab_set_implementer",
            "collab_wait_my_turn(session_id, \"codex\", 60)",
        ]:
            if snippet not in codex_cmd_text:
                err(f".codex-plugin/commands/collab.md: missing {snippet!r}")

        for prompt_name, required in [
            ("collab-plan-draft.md", "selected implementer"),
            ("collab-plan-review.md", "collab_wait_my_turn(session_id, \"codex\", 60)"),
            ("collab-global-review.md", "task_list` is null"),
            ("collab-recovery.md", "topic `final_review`"),
            ("collab-batch-impl.md", "collab_wait_my_turn(session_id, \"codex\", 60)"),
        ]:
            prompt = ROOT / ".codex-plugin" / "prompts" / prompt_name
            if prompt.exists() and required not in prompt.read_text():
                err(f"{prompt.relative_to(ROOT)}: missing required recovery/dispatch contract {required!r}")

    check_failure_prefixes()
    check_no_uninstalled_skill_references()
    check_evaluate_issue_surfaces()
    check_review_diff_fallback_contract()
    check_pr_base_resolution_contract()
    check_review_diff_trigger_detection_contract()

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
