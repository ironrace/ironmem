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
# Per-prompt content pins for the Codex phase prompts. These are the ONLY
# content gate those files have: `lint_template()` globs
# `.claude-plugin/prompts/collab-turn-*.md` and never reads `.codex-plugin/`,
# and the Rust packaging test checks bytes, `$ARGUMENTS` and
# `collab_wait_my_turn` only. They used to live inside the `else:` branch of
# `if not CODEX_COMMAND.exists():`, which meant the whole fixture suite — which
# copies `.codex-plugin/prompts` but not `.codex-plugin/commands` — never ran a
# single one of them, and an inverted protocol contract linted green. Nothing
# here reads the command file, so nothing here may be gated on it.
#
# Phase names are deliberately NOT pinned here: `check_precondition_phase_names`
# derives them from `phase.rs`, and a hardcoded copy in this list would simply
# rot in lockstep with the prompt it is supposed to guard.
CODEX_PROMPT_CONTRACTS = [
    ("collab-plan-draft.md", "selected implementer"),
    ("collab-plan-review.md", "collab_wait_my_turn(session_id, \"codex\", 60)"),
    ("collab-global-review.md", "task_list` is null"),
    ("collab-recovery.md", "topic `final_review`"),
    ("collab-batch-impl.md", "collab_wait_my_turn(session_id, \"codex\", 60)"),
    # The five reversed-role pilot prompts. Each pins its send (or explicit
    # no-send) contract and the by-reference dereference that turn depends on —
    # without these, everything semantic in the file can be deleted with both
    # gates green.
    ("collab-plan-synthesis.md",
     "Send exactly one `collab_send` with sender `codex`, topic `canonical`,"),
    ("collab-plan-synthesis.md", "get_drawer(id=<message.drawer_id>)"),
    ("collab-plan-finalize.md", "**This turn sends nothing.**"),
    ("collab-plan-finalize.md", "get_drawer(id=<canonical_plan_ref.drawer_id>)"),
    ("collab-plan-finalize.md",
     'add_drawer(wing="ironrace-memory", room="collab-drafts",'),
    ("collab-task-list.md",
     "Send `collab_send` with sender `codex`, topic `task_list`,"),
    ("collab-task-list.md", "SHA-256 equals both `final_plan_ref.hash` and"),
    ("collab-review-local.md",
     "Send `collab_send` with sender `codex`, topic `review_local`,"),
    ("collab-review-local.md", "`review_local=reduced`"),
    ("collab-final-review.md",
     "**This turn sends nothing and opens no PR.**"),
    ("collab-final-review.md", "get_drawer(id=<task_list_ref.drawer_id>)"),
    ("collab-final-review.md", '{"title":"<title>","body":"<body>"}'),
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
# Every protocol topic that is dispatched as a *turn* must have a template on
# BOTH harnesses. Role reversal is only real if a pilot=codex session has a
# Codex prompt for each lead turn and a Claude prompt for each copilot turn;
# a topic wired on one side only is invisible until dispatch, where it fails
# as "missing template" mid-session. Keyed by topic -> (claude, codex).
TURN_TOPIC_TEMPLATES = {
    "draft": ("collab-turn-plan-draft.md", "collab-plan-draft.md"),
    "canonical": ("collab-turn-plan-synthesis.md", "collab-plan-synthesis.md"),
    "review": ("collab-turn-plan-review.md", "collab-plan-review.md"),
    "final": ("collab-turn-plan-finalize.md", "collab-plan-finalize.md"),
    "task_list": ("collab-turn-task-list.md", "collab-task-list.md"),
    "implementation_done": ("collab-turn-code-implement.md",
                            "collab-batch-impl.md"),
    "review_local": ("collab-turn-review-local.md", "collab-review-local.md"),
    "review_fix_global": ("collab-turn-review-fix-global.md",
                          "collab-global-review.md"),
    "final_review": ("collab-turn-final-review.md", "collab-final-review.md"),
}
# `failure_report` is not a turn: it is the error completion available to
# several turns and is never dispatched on its own, so it has no template of
# its own on either harness. Codex's recovery override lives in
# collab-recovery.md, which is registered in CODEX_PROMPTS directly.
NON_TURN_TOPICS = {"failure_report"}
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
    "collab-turn-plan-review.md": [
        "PlanCodexReviewPending",
        # The one-recv rule, wrapped as the template ships it.
        "Do not call `collab_recv` again\n   after it acknowledges.",
        "get_drawer(id=<canonical_plan_ref.drawer_id>)",
        "exactly one copilot plan-review pass",
        "Send exactly once",
        "The canonical plan is your only review input.",
    ],
    "collab-turn-review-fix-global.md": [
        "CodeReviewFixGlobalPending",
        "/ultrareview-local",
        "the payload carries only",
        "Send exactly once:",
        # The recovery owner's preserved working tree is the only copy of the
        # interrupted work; a fetch/checkout/reset before inspecting it is
        # unrecoverable data loss.
        "preserve and inspect the working-tree diff *before* any fetch",
        # That snippet lives in the recovery-owner paragraph, and the recovery
        # owner never runs the reset. The agent that does is the NORMAL-turn
        # owner, whose hazard is the opposite one: a prior turn that died hard
        # (OOM, container kill, sandbox teardown) never sent `failure_report`,
        # so `pending_failure` stays null, so the next dispatch correctly
        # self-classifies as a normal turn — and resets away the only copy of
        # the uncommitted fixes with nothing downstream registering the loss.
        # The porcelain precondition must therefore bind unconditionally, not
        # on `pending_failure`.
        "`git status --porcelain` to be empty regardless of `pending_failure`",
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


def check_topic_template_completeness() -> None:
    """Every turn topic must have a template on BOTH harnesses."""
    mapped = set(TURN_TOPIC_TEMPLATES)
    # A new topic must be classified deliberately. Falling through as
    # "unmapped" would make this whole check pass vacuously for it.
    for topic in sorted(VALID_TOPICS - mapped - NON_TURN_TOPICS):
        err(f"topic {topic!r} is in VALID_TOPICS but is neither mapped in "
            f"TURN_TOPIC_TEMPLATES nor declared a non-turn topic — decide "
            f"which, and add the missing harness template if it is a turn")
    for topic in sorted((mapped | NON_TURN_TOPICS) - VALID_TOPICS):
        err(f"topic {topic!r} is mapped/exempted but is not in VALID_TOPICS")
    for topic in sorted(mapped & NON_TURN_TOPICS):
        err(f"topic {topic!r} is both mapped and exempted — pick one")

    codex_registered = {p.name for p in CODEX_PROMPTS}
    codex_dir = ROOT / ".codex-plugin" / "prompts"
    for topic, (claude_name, codex_name) in sorted(TURN_TOPIC_TEMPLATES.items()):
        if not (PROMPTS / claude_name).exists():
            err(f"topic {topic!r}: missing Claude template {claude_name}")
        if not (codex_dir / codex_name).exists():
            err(f"topic {topic!r}: missing Codex prompt {codex_name}")
        expected = EXPECTED_TEMPLATES.get(claude_name)
        if expected is None:
            err(f"topic {topic!r}: Claude template {claude_name} is not "
                f"registered in EXPECTED_TEMPLATES, so it is never installed")
        elif topic not in expected["topics"]:
            err(f"topic {topic!r}: {claude_name} declares topics "
                f"{expected['topics']}, which does not include it")
        if codex_name not in codex_registered:
            err(f"topic {topic!r}: Codex prompt {codex_name} is not "
                f"registered in CODEX_PROMPTS, so it is never linted")


def check_codex_prompt_contracts() -> None:
    """Per-prompt content pins for the Codex phase prompts.

    Runs unconditionally: nothing in `CODEX_PROMPT_CONTRACTS` reads
    `.codex-plugin/commands/collab.md`, so nothing here may be skipped when
    that file is absent. A prompt that is missing entirely is already reported
    by the `CODEX_PROMPTS` existence loop.
    """
    for prompt_name, required in CODEX_PROMPT_CONTRACTS:
        prompt = ROOT / ".codex-plugin" / "prompts" / prompt_name
        if prompt.exists() and required not in prompt.read_text():
            err(f"{prompt.relative_to(ROOT)}: missing required "
                f"recovery/dispatch contract {required!r}")


def check_installer_covers_templates() -> None:
    """Every registered template must also be installed.

    The three other registration surfaces are lint-enforced against each
    other, but a template missing from REQUIRED_CLAUDE_PROMPTS still lints
    green — it is simply never copied to ~/.claude/prompts/, and the gap
    surfaces mid-session as "missing template" at dispatch. Same for the
    Codex prompts and REQUIRED_CODEX_PROMPTS.
    """
    installer = ROOT / "scripts" / "install-ironmem.sh"
    if not installer.exists():
        err("scripts/install-ironmem.sh: missing installer")
        return
    text = installer.read_text()
    for array, required in (
        ("REQUIRED_CLAUDE_PROMPTS", {n[:-3] for n in EXPECTED_TEMPLATES}),
        ("REQUIRED_CODEX_PROMPTS", {p.stem for p in CODEX_PROMPTS}),
    ):
        _, sep, rest = text.partition(f"{array}=(")
        if not sep:
            err(f"scripts/install-ironmem.sh: missing {array} array")
            continue
        listed = {line.strip() for line in rest.partition(")")[0].splitlines()}
        for name in sorted(required - listed):
            err(f"scripts/install-ironmem.sh: {array} is missing {name!r} — "
                f"it is registered for lint but would never be installed")


# ---- preconditions <-> phase.rs wire-name cross-check -----------------------
#
# `collab_status` serializes `phase` through `Display`, which forwards to
# `Phase::wire_name`. Two variants were genericized in #246 while their wire
# strings stayed frozen for stored-session compatibility
# (`PlanCopilotReviewPending` -> "PlanCodexReviewPending",
# `PlanFinalizePending` -> "PlanClaudeFinalizePending"). Every turn template
# tells its worker to compare `phase` against the name on its `preconditions:`
# line and to return a blocker without sending if the comparison fails — so a
# template naming the Rust variant instead of the wire string fails closed on
# every single dispatch, silently and forever. Nothing else in the toolchain
# compares the two files.
PHASE_RS = ROOT / "crates" / "ironmem" / "src" / "collab" / "phase.rs"
# One arm of the exhaustive `wire_name` match: `Self::Variant => "Wire",`.
WIRE_NAME_ARM_RE = re.compile(
    r'Self::([A-Za-z0-9_]+)\s*=>\s*"([A-Za-z0-9_]+)"')
# `phase == PlanLocked`, `phase != CodingFailed`. A template with no phase
# clause at all (collab-turn-submit.md) is legitimate and yields nothing.
PRECONDITION_PHASE_RE = re.compile(r'phase\s*[=!]=\s*([A-Za-z0-9_]+)')
# The Codex prompts carry no `preconditions:` frontmatter — they state the
# phase they own in prose, in exactly two shapes (verified against all ten):
#   "This prompt is only for `CodeImplementPending` when ..."
#   "This prompt is only for a recoverable `A` or `B` turn ..."
#   "This prompt is only for the `PlanLocked` bridge ..."
#   "if phase is not `PlanSynthesisPending` or Codex is not current owner ..."
#   "act only when phase is `CodeImplementPending`, implementer is `codex` ..."
#   "and the phase is `A` or `B`; otherwise report ..."
# Both shapes wrap across lines in the shipped prompts
# (`collab-plan-draft.md`, `collab-plan-review.md`), so this is matched against
# a whitespace-flattened copy of the file. `phase is not yours` names no phase
# and deliberately does not match.
CODEX_PHASE_GUARD_RE = re.compile(
    r'(?:prompt is only for|phase is)'
    r'((?:\s+(?:not|a|an|the|recoverable))*'
    r'\s+`[A-Za-z0-9_]+`(?:\s+or\s+`[A-Za-z0-9_]+`)*)')
BACKTICKED_NAME_RE = re.compile(r'`([A-Za-z0-9_]+)`')


def parse_wire_names() -> dict[str, str]:
    """Rust variant -> wire string, parsed out of `Phase::wire_name`.

    Derived from phase.rs rather than duplicated here: a hardcoded copy is
    exactly the thing that rots next. Scoped to the `wire_name` body because
    `expected_event` has identical arm syntax but maps variants to event
    names, and admitting those would make the check permissive in the only
    direction that matters. Every failure to parse is reported: a check that
    silently finds zero phases would pass everything.
    """
    if not PHASE_RS.exists():
        err(f"{PHASE_RS}: cannot cross-check preconditions phase names — "
            f"collab/phase.rs not found")
        return {}
    _, sep, rest = PHASE_RS.read_text().partition("fn wire_name")
    if not sep:
        err("collab/phase.rs: no `fn wire_name` found — the phase-name "
            "cross-check would pass vacuously, so it fails instead")
        return {}
    # The match arms end with the function's closing brace at 4-space indent.
    arms = dict(WIRE_NAME_ARM_RE.findall(rest.partition("\n    }")[0]))
    if not arms:
        err("collab/phase.rs: no wire_name match arms parsed — the "
            "phase-name cross-check would pass vacuously, so it fails instead")
    return arms


def codex_phase_names(text: str) -> set[str]:
    """Phase names a Codex phase prompt guards on, from its prose guards."""
    flat = " ".join(text.split())
    names: set[str] = set()
    for m in CODEX_PHASE_GUARD_RE.finditer(flat):
        names.update(BACKTICKED_NAME_RE.findall(m.group(1)))
    return names


def check_codex_phase_names(wire_by_variant: dict[str, str]) -> None:
    """The Codex half of the phase.rs cross-check.

    Without this, renaming a wire string in phase.rs flags the Claude template
    and leaves the Codex prompt stale — under `pilot=codex` that turn then
    fails its own guard on every dispatch, so the session stalls with no
    failure recorded and looks like a hang rather than a rename. The names used
    to be pinned as hardcoded literals in `CODEX_PROMPT_CONTRACTS`, which is
    the same duplicated copy `parse_wire_names()` exists to abolish: the stale
    prompt and the stale literal agreed with each other and the gate passed.
    """
    emitted = set(wire_by_variant.values())
    for path in CODEX_PROMPTS:
        if not path.exists():
            continue  # already reported as a missing Codex phase prompt
        rel = path.relative_to(ROOT)
        names = codex_phase_names(path.read_text())
        # Every registered Codex phase prompt owns at least one phase and says
        # so. A prompt that drops or reformats its guard would otherwise become
        # silently unchecked — the exact failure mode this whole check exists
        # to close.
        if not names:
            err(f"{rel}: no phase guard found — every registered Codex phase "
                f"prompt must name the phase it owns in the "
                f"'only for `<WireName>`' / 'phase is `<WireName>`' form so "
                f"the name is verified against phase.rs")
            continue
        for name in sorted(names - emitted):
            hint = (f"use the wire name {wire_by_variant[name]!r}"
                    if name in wire_by_variant
                    else "it is not a Phase wire name at all")
            err(f"{rel}: phase guard names phase {name!r}, which phase.rs "
                f"never emits — {hint}; as written the guard fails on every "
                f"dispatch and the turn never sends")


def check_precondition_phase_names() -> None:
    """Every phase a template compares against must be one the server emits.

    Scans the WHOLE template, not just the `preconditions:` value: that value
    is metadata, while the comparison a worker actually executes is the prose
    State-discovery step restating `phase == <WireName>`. Checking only the
    frontmatter would guard the copy nobody runs — a Rust variant name in the
    body alone would pass this check, satisfy the `REQUIRED_TEMPLATE_SNIPPETS`
    substring pin on the strength of the frontmatter occurrence, and still
    fail closed on every dispatch.

    Covers both harnesses: role reversal means a `pilot=codex` session runs the
    Codex prompt for the same turn, and its guard rots exactly as easily.
    """
    wire_by_variant = parse_wire_names()
    if not wire_by_variant:
        return
    emitted = set(wire_by_variant.values())
    check_codex_phase_names(wire_by_variant)
    for path in sorted(PROMPTS.glob("collab-turn-*.md")):
        text = path.read_text()
        fm = parse_frontmatter(text)
        if fm is None:
            continue  # lint_template already reports the missing frontmatter
        names = set(PRECONDITION_PHASE_RE.findall(text))
        # A precondition phrased as prose (`phase is PlanLocked`) matches no
        # name and would be silently unchecked, indistinguishable from the
        # legitimately phase-free collab-turn-submit.md.
        preconditions = fm.get("preconditions", "")
        if "phase" in preconditions and not PRECONDITION_PHASE_RE.search(preconditions):
            err(f"{path.name}: preconditions mentions a phase but names none "
                f"in the `phase == <WireName>` form this lint can check — "
                f"rewrite it in that form so the name is verified against "
                f"phase.rs")
        for name in sorted(names - emitted):
            hint = (f"use the wire name {wire_by_variant[name]!r}"
                    if name in wire_by_variant
                    else "it is not a Phase wire name at all")
            err(f"{path.name}: preconditions names phase {name!r}, which "
                f"phase.rs never emits — {hint}; as written the precondition "
                f"check fails on every dispatch and the turn never sends")


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
        err(f"collab.md: dispatch matrix must reference exactly the "
            f"{len(expected_names)} required collab-turn templates")
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

    check_codex_prompt_contracts()
    check_failure_prefixes()
    check_no_uninstalled_skill_references()
    check_evaluate_issue_surfaces()
    check_review_diff_fallback_contract()
    check_topic_template_completeness()
    check_installer_covers_templates()
    check_precondition_phase_names()
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
