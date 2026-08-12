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
COMMANDS_DIR = ROOT / ".claude-plugin" / "commands"
CODEX_PROMPTS_DIR = ROOT / ".codex-plugin" / "prompts"
CODEX_COMMANDS_DIR = ROOT / ".codex-plugin" / "commands"
ULTRAREVIEW_SRC = ROOT / ".claude-plugin" / "workflows" / "ultrareview.js"
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
                        "ARTIFACT_REF", "ARTIFACT_HASH", "MODE", "SENDER"}
REQUIRED_FM = {"turn", "tier", "model", "topics", "preconditions"}
VALID_TIERS = {"planning", "review", "mechanical"}
VALID_MODELS = {"opus", "sonnet", "haiku", "default"}
VALID_TOPICS = {"draft", "canonical", "review", "final", "task_list",
                "implementation_done", "review_local", "review_fix_global",
                "final_review", "failure_report", "orphan_recovered"}
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
        "topics": ["review_fix_global", "failure_report", "orphan_recovered"],
    },
    "collab-turn-review-local.md": {
        "turn": "review_local",
        "tier": "review",
        "model": "opus",
        "topics": ["review_local", "failure_report", "orphan_recovered"],
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
# Neither advances a turn. `failure_report` parks the phase and hands over;
# `orphan_recovered` does not even do that — the server records it and returns
# before building an event, so phase, owner and both recovery counters are
# untouched. See ORPHAN_RECOVERED_TOPIC in collab_session.rs.
NON_TURN_TOPICS = {"failure_report", "orphan_recovered"}
REQUIRED_TEMPLATE_SNIPPETS = {
    "collab-turn-plan-synthesis.md": [
        "first auto-ack response",
        "get_drawer(id=<message.drawer_id>)",
        "Do not call `collab_recv` again after it acknowledges.",
        "`full:true` is compatibility-only",
    ],
    "collab-turn-task-list.md": [
        "Timebox: <=20 minutes",
        "more than 15 tasks",
        "PlanLocked is pre-coding",
        "plan_file_path",
        # The `PlanLocked` bridge is sender-parameterized for exactly the same
        # reason the submit template is: `PublishFinal` does not reassign
        # ownership (`state_machine/mod.rs`), so under `pilot=codex` this turn
        # is entered with `current_owner == codex` and `SubmitTaskList`
        # requires `pilot(session)`. A hardcoded `sender="claude"` here is
        # rejected by the server, and because `PlanLocked` is not
        # `Phase::is_coding_active()` there is no `failure_report` escape —
        # the session simply dead-ends. Pin the send site, the authorization
        # guard, its ABORT enforcement, and the pilot-generic invariant.
        'collab_send(sender="$SENDER", topic="task_list",',
        'Verify `$SENDER` against `collab_status.current_owner`',
        'MUST NOT be\n   substituted with your own identity',
        'equal `current_owner`, ABORT — do not send anything — and report the',
        'always the pilot, which under `pilot == "codex"` is `codex`',
    ],
    "collab-turn-plan-finalize.md": [
        "Timebox: <=20 minutes",
        "at most 15 tasks",
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
        # Pin all FOUR collab_send call sites to the post-gate `$SENDER`
        # (never a hardcoded "claude") — every snippet below STARTS at its
        # site's `collab_send(sender="$SENDER",` so the sender is part of the
        # pin, then extends into that site's distinct payload so it binds to
        # one site only. Snippets that started at `topic="failure_report",`
        # asserted nothing at all about the sender: dropping `sender=` from
        # the `pr_create_failed` send left it senderless (and so also clear of
        # the FORBIDDEN `sender="claude"` pin) with this lint still green.
        'collab_send(sender="$SENDER", topic="final_review",',
        'collab_send(sender="$SENDER", topic="final",',
        'collab_send(sender="$SENDER",\n  topic="failure_report",\n'
        '  content=<JSON {"coding_failure":"pr_create_failed:',
        'collab_send(sender="$SENDER",\n  topic="failure_report", content=<JSON'
        ' {"coding_failure":\n  "approved_artifact_unfetchable:',
        # The $SENDER authorization guard itself: without this pin, deleting
        # the whole verification/abort/recovery-invariant block (state
        # discovery step 2) leaves every collab_send pin above still green,
        # since the sends themselves are untouched by removing the guard
        # that gates them.
        'Verify `$SENDER` against `collab_status.current_owner`',
        'MUST NOT be\n   substituted with your own identity',
        'may\n     legitimately be the recovery owner rather than the pilot',
        # ...and the guard's ENFORCEMENT clause, which the rationale pins
        # above do not imply. Rewriting the abort to "fall back to
        # `current_owner` and continue with the send" left all three of them
        # matching and the whole lint green, silently converting the hard
        # abort into the identity fallback this branch exists to remove.
        'equal `current_owner`, ABORT — do not send anything — and report the',
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
    "collab-turn-review-local.md": [
        "CodeReviewLocalPending",
        # The reset guard itself is NOT pinned here. Substring pins assert that
        # words exist somewhere in the file, not that the reset obeys them —
        # parking these literals in an HTML comment while restoring an
        # unconditional reset keeps every pin matching. `check_reset_guards`
        # enforces the guard structurally instead; see RESET_GUARD_SURFACES.
    ],
    "collab-turn-review-fix-global.md": [
        "CodeReviewFixGlobalPending",
        "/ultrareview-local",
        "the payload carries only",
        "Send exactly once:",
        # The reset guard is enforced structurally by `check_reset_guards`,
        # not by substring pins — see RESET_GUARD_SURFACES for why.
    ],
}
# Prompts that instruct a worker to `git reset --hard`. Each one is a place a
# turn can destroy the only copy of uncommitted work, so each is checked for a
# guard by structure rather than by wording.
#
# Two distinct hazards, both of which the guard must cover:
#
#   1. The RECOVERY owner holds the interrupted turn's preserved working tree.
#      It must be told to skip the sync outright; a fetch/checkout/reset before
#      inspecting that tree is unrecoverable data loss.
#   2. The NORMAL-turn owner faces the opposite case. A turn that died hard
#      (OOM, container kill, sandbox teardown) never sent `failure_report`, so
#      `pending_failure` stays null, so the next dispatch correctly
#      self-classifies as a normal turn — and resets away the only copy of the
#      uncommitted work with nothing downstream registering the loss. The
#      cleanliness precondition must therefore bind unconditionally, never on
#      `pending_failure`.
#
# Why this is not a REQUIRED_TEMPLATE_SNIPPETS entry: substring pins assert
# that words appear somewhere in the file, not that the reset obeys them.
# Issue #254's exact pre-fix template passes a pure-substring gate if the
# pinned literals are parked in an HTML comment, and a template may state the
# precondition and then override it ("if dirty, note it and continue anyway").
# Both were verified green against a substring-only pin before this check
# existed. Ordering and enforcement are the properties that matter, so they are
# what gets checked.
RESET_GUARD_SURFACES = (
    ".claude-plugin/prompts/collab-turn-review-local.md",
    ".claude-plugin/prompts/collab-turn-review-fix-global.md",
    ".codex-plugin/prompts/collab-review-local.md",
    # `collab-global-review.md` is the Codex mirror of
    # `collab-turn-review-fix-global.md` and owns `CodeReviewFixGlobalPending`
    # under the DEFAULT `pilot=claude`, so it is on the live default path.
    # `collab-batch-impl.md` is `CodeImplementPending` under
    # `implementer=codex` — the phase carrying the most uncommitted work and
    # the highest hard-kill probability, since it runs a full `iron-build`
    # batch. Both gated preservation on `pending_failure` with no cleanliness
    # check until #257.
    ".codex-plugin/prompts/collab-global-review.md",
    ".codex-plugin/prompts/collab-batch-impl.md",
)
# The cleanliness precondition, in two forms. The bare form locates it; the
# qualified form is what the file must actually carry.
#
# The qualifier is not decoration. Re-gating the precondition on
# `pending_failure` ("...to be empty only when `pending_failure` is non-null")
# IS issue #254 — the hard-killed turn never sent a `failure_report`, so
# `pending_failure` is null precisely when the uncommitted work is at risk. The
# positional checks below cannot see that mutation at all: it leaves the
# precondition ahead of the reset, the enforcement clause intact and the reset
# conditioned on a clean tree, so all four pass while the guard binds in
# exactly the cases where it is not needed. This literal is therefore the one
# part of the guard that is pinned by wording, and deliberately so.
def flex(phrase: str) -> re.Pattern:
    """A fixed phrase, matched across any run of whitespace.

    Every clause below is prose inside a wrapped paragraph, so its line breaks
    move whenever a word is added anywhere earlier in the sentence. Patterns
    that hardcoded the wrap points present when they were written
    (`do\\s*\\n?\\s*not run ...`) went silently unmatched the first time a
    sentence was extended — reporting the guard as missing when it was only
    reflowed. Whitespace-insensitive matching is the whole point of checking
    prose by regex rather than by literal.
    """
    return re.compile(r"\s+".join(re.escape(w) for w in phrase.split()))


RESET_GUARD_BARE_PRECONDITION_RE = flex("`git status --porcelain` to be empty")
RESET_GUARD_PRECONDITION_RE = flex(
    "`git status --porcelain` to be empty regardless of `pending_failure`")
# `--porcelain` reports the working tree and the index. It is silent about work
# that was committed but never pushed, and `git reset --hard <last_head_sha>`
# discards those commits exactly as thoroughly as it discards an unstaged edit.
# A turn that got far enough to commit before dying — the common case, since
# `iron-build` commits per task — therefore leaves a tree the cleanliness check
# calls clean and the reset then destroys. The ahead-count must be checked
# alongside it, not instead of it: the two cover different halves of the same
# hazard.
RESET_GUARD_UNPUSHED_RE = flex(
    "`git rev-list <last_head_sha>..HEAD` to be empty")
RESET_GUARD_ENFORCEMENT_RE = flex("do not run `git reset --hard`")
# Every one of these turns commits and pushes. `git fetch` + `git reset --hard`
# does not move the checkout, so a turn that inherits whatever branch the
# previous one left behind resets THAT branch to the session head and pushes
# from it. `collab-turn-review-local.md` ordered the reset with no checkout at
# all. The wording varies across the five surfaces ("`git checkout <branch>`",
# "checkout the session branch", "checkout the session `branch`"), so this
# matches the verb and its object rather than any one phrasing.
RESET_GUARD_CHECKOUT_RE = re.compile(
    r"(`git checkout <branch>`|checkout[^.]{0,40}?branch)", re.IGNORECASE)
RESET_GUARD_CONDITIONAL_RE = re.compile(
    flex("Only when the worktree is clean").pattern, re.IGNORECASE)
RESET_GUARD_RECOVERY_SKIP_RE = re.compile("|".join(
    flex(p).pattern for p in ("skip the sync", "skip this step entirely",
                              "skip only the")), re.IGNORECASE)
# Not every `git reset --hard` in a template is an instruction to reset. Two
# kinds are the guard talking about the reset rather than ordering one, and
# both legitimately precede the precondition:
#   - the prohibition itself ("do not run `git reset --hard`")
#   - the recovery owner's skip list ("Skip only the `git fetch`, the
#     checkout, and the `git reset --hard` in the next paragraph")
# Exempting them by surrounding context keeps the ordering rule strict for
# every occurrence that IS an instruction, rather than weakening it to "the
# last one wins" — which would let a guarded reset be followed by an
# unguarded one.
# Finding a dirty worktree on a NORMAL turn is evidence a previous turn died
# without reporting. Preserving the work is necessary but not sufficient: if
# the turn then completes normally and says nothing, the session's own history
# shows an ordinary turn and the lost turn is invisible. Every prompt that
# guards a reset must also report the incident.
RESET_GUARD_REPORT_RE = re.compile(r"topic=\"?orphan_recovered", re.MULTILINE)
RESET_MENTION_EXEMPT_RE = re.compile(
    "(" + RESET_GUARD_ENFORCEMENT_RE.pattern + "|"
    + flex("Skip only the").pattern
    + r"[^.]{0,80}?`git reset --hard`)", re.IGNORECASE)
# How far before a reset instruction its "only when the worktree is clean"
# clause may sit. The conditional is checked per-reset rather than
# file-globally: a single conditional anywhere in the file satisfied a
# file-global search no matter how many further resets followed it, so
# appending `Later: just `git reset --hard <last_head_sha>` no matter what.`
# to a fully guarded template linted green. In every shipped surface the
# clause reads "Only when the worktree is clean, `git reset --hard ...`" —
# three characters of separator, possibly wrapped — so this bound is generous
# enough for rewrapping and far too tight for an unrelated conditional
# elsewhere in the file to be borrowed by a later reset.
RESET_CONDITIONAL_PROXIMITY = 40
HTML_COMMENT_RE = re.compile(r"<!--.*?-->", re.DOTALL)
FORBIDDEN_TEMPLATE_SNIPPETS = {
    "collab-turn-plan-synthesis.md": [
        "read Codex's draft",
    ],
    "collab-turn-plan-finalize.md": [
        "read `canonical_plan`",
        "read Codex's review notes",
    ],
    "collab-turn-submit.md": [
        # A hardcoded sender identity bypasses the post-gate $SENDER
        # authorization check and lets any owner submit under a stale
        # "claude" identity — see collab-turn-submit.md's own "$SENDER is
        # authoritative" invariant.
        'sender="claude"',
    ],
    "collab-turn-task-list.md": [
        'sender="claude"',
    ],
}
# The literal pins above name the exact regression that shipped, and their
# diagnostics quote it verbatim. They are still only literals: `sender='claude'`,
# `sender=claude` and `sender="Claude"` are the same bug and all passed. This
# regex generalizes them over quoting and capitalization for the two templates
# whose sends must go out as `sender="$SENDER"`.
HARDCODED_SENDER_RE = re.compile(r'sender\s*=\s*["\']?\s*claude\b', re.IGNORECASE)
SENDER_PARAMETERIZED_TEMPLATES = ("collab-turn-submit.md",
                                  "collab-turn-task-list.md")
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
    "more than 15",
    "Child issues:",
    "Parent: #<number>",
    "advisory-only",
    "Split-child-key:",
    "Split-parent-key:",
]
# Every planning surface that states the collab execution ceiling has an
# explicit 15/16 contract. This is intentionally more specific than the
# evaluator and template checks above: those guards catch a few shared phrases,
# while this map keeps every user-facing mirror and the PlanLocked enforcement
# line synchronized with the server's 15-task boundary.
TASK_BUDGET_SURFACE_CONTRACTS = {
    DOC: [
        "**1–15 execution tasks**",
        "1–15-task collab session",
        "A plan projected to require 16 or more tasks",
        "more than 15 tasks",
        "`> 15` task-count check",
        "A 16+ task issue",
        "**1–15** strictly ordered entries",
    ],
    EVALUATE_ISSUE_DOC: [
        "An estimate above 15 requires `SPLIT`.",
        "more than 15 independent execution tasks",
        "**1–15** execution tasks",
        "1–15 task estimate",
        "An estimate above 15 tasks always yields `SPLIT`",
    ],
    COMMAND: [
        "at most 15 tasks",
        "If it would need 16 or more",
        "`> 15` task-count check",
        "a 16-task plan",
        "more than 15 `### Task ` headings",
    ],
    EVALUATE_ISSUE_CLAUDE: [
        "mandatory SPLIT above 15 tasks",
        "above 15 requires `SPLIT`",
        "more than 15 independent execution tasks",
        "1–15-task estimate",
        "1–15 task estimate",
        "estimate above 15 tasks always yields `SPLIT`",
    ],
    PROMPTS / "collab-turn-plan-review.md": [
        "capped at 15 execution tasks",
        "credibly needs 16 or more",
    ],
    PROMPTS / "collab-turn-plan-draft.md": [
        "at most 15 execution tasks",
    ],
    PROMPTS / "collab-turn-plan-synthesis.md": [
        "at most 15 execution tasks",
    ],
    PROMPTS / "collab-turn-plan-finalize.md": [
        "at most 15 tasks",
        "needs 16 or more",
        "heading count is at most 15",
    ],
    PROMPTS / "collab-turn-task-list.md": [
        "heading count is at most 15",
        "more than 15 tasks",
    ],
    CODEX_COMMAND: [
        "1–15 execution tasks",
        "work needs 16 or more",
    ],
    EVALUATE_ISSUE_CODEX: [
        "mandatory SPLIT above 15 tasks",
        "above 15 requires `SPLIT`",
        "more than 15 independent execution tasks",
        "1–15-task estimate",
        "1–15 task estimate",
        "estimate above 15 tasks always yields `SPLIT`",
    ],
    ROOT / ".codex-plugin" / "prompts" / "collab-plan-draft.md": [
        "at most 15 execution tasks",
    ],
    ROOT / ".codex-plugin" / "prompts" / "collab-plan-synthesis.md": [
        "at most 15 execution tasks",
    ],
    ROOT / ".codex-plugin" / "prompts" / "collab-plan-review.md": [
        "capped at 15 execution tasks",
        "credibly needs 16 or more",
    ],
    ROOT / ".codex-plugin" / "prompts" / "collab-plan-finalize.md": [
        "at most 15 tasks",
        "needs 16 or more",
        "at least 1 and at most 15",
    ],
    ROOT / ".codex-plugin" / "prompts" / "collab-task-list.md": [
        "at most 15",
        "more than 15 tasks",
    ],
}
# Most positive phrases occur once, so changing their number removes the sole
# required match. These four phrases are deliberately repeated in their live
# surface; pin their exact cardinality so changing only one copy to any wrong
# value (not just the legacy 10/11 boundary) cannot hide behind another copy.
TASK_BUDGET_SURFACE_CONTRACT_COUNTS = {
    (DOC, "more than 15 tasks"): 3,
    (EVALUATE_ISSUE_DOC, "1–15 task estimate"): 2,
    (EVALUATE_ISSUE_CLAUDE, "1–15 task estimate"): 2,
    (EVALUATE_ISSUE_CODEX, "1–15 task estimate"): 2,
}
FINALIZE_ABORT_SURFACE_CONTRACTS = {
    PROMPTS / "collab-turn-plan-finalize.md": [
        "any blocker that prevents staging a valid final plan must call "
        "`collab_end(session_id=$SESSION_ID, agent=\"claude\")` exactly once",
    ],
    ROOT / ".codex-plugin" / "prompts" / "collab-plan-finalize.md": [
        "any blocker that prevents staging a valid final plan must call "
        "`collab_end(session_id, agent=\"codex\")` exactly once",
    ],
    PROMPTS / "collab-turn-submit.md": [
        "call `collab_end(session_id=$SESSION_ID, agent=\"$SENDER\")` "
        "before returning the blocker",
    ],
    COMMAND: [
        "A finalization `blocker:` is terminal: the worker must have ended "
        "the session",
    ],
    DOC: [
        "A finalization `blocker:` is terminal: the worker must have ended "
        "the session",
    ],
}
# Positive pins alone cannot detect one stale occurrence when the same current
# phrase appears elsewhere in a surface. Scan only the task-budget surfaces
# above, and only task-budget contexts, so unrelated 10/11 values remain free.
STALE_TASK_BUDGET_PATTERNS = [
    ("10-task issue budget", re.compile(r"\b10-task issue budget\b")),
    ("1–10 task range", re.compile(
        r"\b1[–-]10(?:-task\s+(?:estimate|collab\s+session)|\s+(?:execution\s+tasks?|"
        r"task\s+estimate|tasks?))\b")),
    ("1–10 strictly ordered entries", re.compile(
        r"\*\*1[–-]10\*\*\s+strictly ordered entries\b")),
    ("10-task comparative ceiling", re.compile(
        r"\b(?:more than|at most|above)\s+10(?:\s+(?:independent\s+)?"
        r"(?:execution\s+)?tasks?|\s+requires\b)")),
    ("estimate above 10", re.compile(r"\bestimate above 10(?:\s+tasks?)?\b")),
    ("> 10 task-count", re.compile(r">\s*10`?\s+task-count\b")),
    ("more than 10 task headings", re.compile(
        r"\bmore than 10\s+`### Task `\s+headings\b")),
    ("heading count at most 10", re.compile(r"\bheading count is at most 10\b")),
    ("structure count at most 10", re.compile(r"\bat least 1 and at most 10\b")),
    ("11 or more task/plan/scope", re.compile(
        r"\b(?:needs?|requires?|require|would need)\s+11 or more\b")),
    ("11 or more task/plan/scope", re.compile(
        r"\b11 or more\s+(?:tasks?|plans?|scope)\b")),
    ("11+ task/plan/scope", re.compile(r"\b11\+\s+(?:task|plan|scope)\b")),
    ("11-task plan", re.compile(r"\b11-task\s+plan\b")),
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
# Anchor text for the docs/COLLAB.md heading the `pr_create_failed:` comment
# below points at. Defined once so `check_pr_create_failed_doc_pointer_contract`
# can pin BOTH sides of that pointer — this file's comment and the doc
# heading — against the same string, instead of only pinning the doc side and
# trusting the comment's prose to stay in sync by hand.
PR_CREATE_FAILED_DOC_HEADING = "`pr_create_failed:` stays Terminal"
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
# `pr_create_failed:` stays Terminal deliberately: in normal flow PR creation
# is Claude-worker-only, so a failure here has no live counterpart to hand a
# retry to. Codex opens the PR itself only under the delegated-completion
# recovery override (`.codex-plugin/prompts/collab-recovery.md`,
# `CodeReviewFinalPending`), and there it reports the recoverable
# `network_failed:`/`sandbox_denied:` prefix rather than this one — so that
# exception does not give this prefix a live counterpart either. See
# "`pr_create_failed:` stays Terminal" in docs/COLLAB.md for the full
# rationale and manual recovery steps. (That quoted phrase must stay
# byte-identical to PR_CREATE_FAILED_DOC_HEADING above —
# check_pr_create_failed_doc_pointer_contract pins both against it.)
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


def check_task_budget_surface_contracts() -> None:
    """Keep every task-budget mirror at the 15-task / 16-task split."""
    for path, snippets in TASK_BUDGET_SURFACE_CONTRACTS.items():
        if not path.exists():
            err(f"{path.relative_to(ROOT)}: missing task-budget surface")
            continue
        body = live_text(path)
        for snippet in snippets:
            match_count = len(flex(snippet).findall(body))
            if match_count == 0:
                err(f"{path.relative_to(ROOT)}: missing task-budget contract "
                    f"{snippet!r}")
                continue
            expected_count = TASK_BUDGET_SURFACE_CONTRACT_COUNTS.get(
                (path, snippet), 1)
            if match_count != expected_count:
                err(f"{path.relative_to(ROOT)}: task-budget contract "
                    f"{snippet!r} expected {expected_count} occurrences, "
                    f"found {match_count}")
        for pattern_name, pattern in STALE_TASK_BUDGET_PATTERNS:
            for match in pattern.finditer(body):
                stale_text = " ".join(match.group(0).split())
                err(f"{path.relative_to(ROOT)}: stale task-budget ceiling "
                    f"{stale_text!r}; required 15/16 contract "
                    f"(pattern: {pattern_name})")

    bridge = command_section(live_text(COMMAND), PLAN_LOCKED_GATE_HEADING)
    enforcement = "more than 15 `### Task ` headings"
    if bridge is None:
        err(".claude-plugin/commands/collab.md: missing PlanLocked bridge "
            "task-budget enforcement section")
    elif not flex(enforcement).search(bridge):
        err(".claude-plugin/commands/collab.md: PlanLocked bridge is missing "
            f"task-budget enforcement {enforcement!r}")


def check_finalize_abort_contracts() -> None:
    """An unfinalizable bounded plan must end, never strand the start slot."""
    for path, snippets in FINALIZE_ABORT_SURFACE_CONTRACTS.items():
        if not path.exists():
            err(f"{path.relative_to(ROOT)}: missing finalize-abort surface")
            continue
        body = live_text(path)
        for snippet in snippets:
            if not flex(snippet).search(body):
                err(f"{path.relative_to(ROOT)}: missing finalize-abort contract "
                    f"{snippet!r}")


def check_review_diff_fallback_contract() -> None:
    """Keep every review entrypoint artifact-first with a raw fallback."""
    for path, snippets in REVIEW_DIFF_FALLBACK_SURFACES.items():
        if not path.exists() or any(snippet not in path.read_text() for snippet in snippets):
            err(f"{path.relative_to(ROOT)}: missing review-diff fallback contract")


def check_reset_guards() -> None:
    """Every `git reset --hard` instruction must be structurally guarded.

    Checked by position, not by wording, so the guard survives rephrasing and
    cannot be satisfied by text that merely mentions it. HTML comments are
    stripped first: parking the required phrases in a comment while leaving an
    unconditional reset in the body is exactly how issue #254's pre-fix
    template passes a substring-only gate.

    Position is checked against EVERY non-exempt reset, not just the first.
    A guard that only has to precede the earliest occurrence is satisfied by a
    fully guarded step 1 followed by a bare reset anywhere later in the file,
    and — since the clauses were otherwise searched file-globally — by demoting
    the whole guard to a trailing "historical note" below an unconditional
    reset. Both linted green while the executable path reset unconditionally.

    The one wording pin that remains is the precondition's
    "regardless of `pending_failure`" qualifier; see
    RESET_GUARD_PRECONDITION_RE for why no positional check can replace it.
    """
    for rel in RESET_GUARD_SURFACES:
        path = ROOT / rel
        if not path.exists():
            err(f"{rel}: missing reset-guard surface")
            continue
        text = HTML_COMMENT_RE.sub("", path.read_text())
        exempt = [m.span() for m in RESET_MENTION_EXEMPT_RE.finditer(text)]
        resets = [m.start() for m in re.finditer(r"git reset --hard", text)
                  if not any(s <= m.start() < e for s, e in exempt)]
        if not resets:
            err(f"{rel}: expected a `git reset --hard` instruction to guard; "
                "if the reset was removed, drop this file from "
                "RESET_GUARD_SURFACES")
            continue
        first_reset = min(resets)
        pre = RESET_GUARD_PRECONDITION_RE.search(text)
        if not RESET_GUARD_BARE_PRECONDITION_RE.search(text):
            err(f"{rel}: no `git status --porcelain` cleanliness precondition "
                "guarding `git reset --hard`")
        elif not pre:
            err(f"{rel}: `git status --porcelain` precondition does not bind "
                "unconditionally — expected \"to be empty regardless of "
                "`pending_failure`\". A turn killed hard never sends a "
                "`failure_report`, so `pending_failure` is null in exactly "
                "the case the guard exists for (issue #254)")
        elif pre.start() > first_reset:
            # Substring membership has no ordering; a precondition stated after
            # the reset it governs is not a precondition.
            err(f"{rel}: `git status --porcelain` precondition appears after "
                "the `git reset --hard` it must guard")
        for label, pattern, diagnostic in (
            ("checkout", RESET_GUARD_CHECKOUT_RE,
             "does not check out the session branch before resetting — this "
             "turn commits and pushes, and `git reset --hard` moves whatever "
             "branch the previous turn happened to leave checked out"),
            ("unpushed-commit", RESET_GUARD_UNPUSHED_RE,
             "cleanliness precondition covers only the working tree — expected "
             "`git rev-list <last_head_sha>..HEAD` to be empty alongside it, "
             "since `--porcelain` is empty for committed-but-unpushed work "
             "that the reset discards just the same"),
            ("enforcement", RESET_GUARD_ENFORCEMENT_RE,
             "cleanliness precondition states no consequence — expected an "
             "explicit \"do not run `git reset --hard`\""),
            ("recovery skip", RESET_GUARD_RECOVERY_SKIP_RE,
             "recovery owner is not told to skip the sync"),
        ):
            match = pattern.search(text)
            if not match:
                err(f"{rel}: {diagnostic}")
            elif match.start() > first_reset:
                err(f"{rel}: the {label} clause appears after the "
                    "`git reset --hard` it must govern")
        # The conditional binds per reset: each instruction needs its own
        # "only when the worktree is clean" immediately ahead of it.
        conditionals = [m.end() for m in RESET_GUARD_CONDITIONAL_RE.finditer(text)]
        unguarded = [r for r in resets if not any(
            0 <= r - end <= RESET_CONDITIONAL_PROXIMITY for end in conditionals)]
        if unguarded:
            err(f"{rel}: {len(unguarded)} of {len(resets)} `git reset --hard` "
                "instructions are not conditioned on a clean worktree — "
                'expected "Only when the worktree is clean" immediately '
                f"before each (first unguarded at offset {min(unguarded)})")
        # Presence only, deliberately unpositioned: the incident is recorded
        # *after* the recovered work is committed, so it legitimately sits on
        # either side of the reset the other clauses are ordered against.
        if not RESET_GUARD_REPORT_RE.search(text):
            err(f"{rel}: a dirty worktree on a normal turn is never reported — "
                "expected a `collab_send` with topic=orphan_recovered")


# The four review turns whose reviewed range is `base_sha..last_head_sha` read
# from `collab_status`, but whose completion event sends the CURRENT HEAD. A
# recovery owner commits the work it recovered, so its HEAD moves past the
# recorded `last_head_sha` — and reviewing the recorded range while sending a
# head beyond it promotes those commits to session head with nobody having read
# them. `collab-batch-impl.md` is deliberately absent: it implements rather than
# reviews, and the head it sends is reviewed afterwards at `review_fix_global`.
#
# Pinned by literal rather than by structure, because the property is a
# statement about which SHA the worker feeds two later steps — there is no
# position or ordering in the file that distinguishes stating it from not. The
# sentence is therefore identical in all four surfaces, so one pin covers them.
REVIEW_RANGE_RECOVERY_SURFACES = (
    ".claude-plugin/prompts/collab-turn-review-local.md",
    ".claude-plugin/prompts/collab-turn-review-fix-global.md",
    ".codex-plugin/prompts/collab-review-local.md",
    ".codex-plugin/prompts/collab-global-review.md",
)
REVIEW_RANGE_RECOVERY_SNIPPETS = [
    "the review range head is your post-recovery `HEAD`, not `last_head_sha`",
    # Naming the substitution site matters as much as naming the rule: the
    # review-input commands below it spell `--head <last_head_sha>` literally,
    # and are themselves pinned verbatim by REVIEW_DIFF_FALLBACK_SURFACES.
    "substitute it for `<last_head_sha>` in the review-input commands below",
    "so you review `<base_sha>..<HEAD>`",
    "makes the recovered work the session head with nobody having read it",
]
# `collab-batch-impl.md`'s fast path returns before the reset, so
# `check_reset_guards` never reaches it — and the state it skips ahead from is
# precisely the post-OOM one: HEAD still at `last_head_sha`, branch correct,
# tree dirty. Nothing is destroyed there, but the batch is then built on top of
# the dead turn's unrecovered work. Matched by proximity so the condition has
# to sit in the fast-path sentence itself, not merely somewhere in the file.
FAST_PATH_RE = re.compile(r"take the fast path")
FAST_PATH_CLEANLINESS_RE = re.compile(r"`git status --porcelain` is empty")
FAST_PATH_PROXIMITY = 200


def check_review_range_after_recovery() -> None:
    """A recovery owner must review the range it is about to send."""
    for rel in REVIEW_RANGE_RECOVERY_SURFACES:
        path = ROOT / rel
        if not path.exists():
            err(f"{rel}: missing review-range surface")
            continue
        text = HTML_COMMENT_RE.sub("", path.read_text())
        for snippet in REVIEW_RANGE_RECOVERY_SNIPPETS:
            if snippet.replace("\n", " ") not in " ".join(text.split()):
                err(f"{rel}: recovered commits are not brought into the "
                    f"reviewed range — missing {snippet!r}")


def check_batch_impl_fast_path_cleanliness() -> None:
    """The implement turn's fast path must require a clean tree too."""
    rel = ".codex-plugin/prompts/collab-batch-impl.md"
    path = ROOT / rel
    if not path.exists():
        err(f"{rel}: missing fast-path surface")
        return
    text = HTML_COMMENT_RE.sub("", path.read_text())
    fast = FAST_PATH_RE.search(text)
    if not fast:
        err(f"{rel}: expected a fast path to guard; if it was removed, drop "
            "this check")
        return
    window = text[fast.end():fast.end() + FAST_PATH_PROXIMITY]
    if not FAST_PATH_CLEANLINESS_RE.search(window):
        err(f"{rel}: the fast path is not conditioned on a clean worktree — "
            "expected \"`git status --porcelain` is empty\" among its "
            "conditions. HEAD at `last_head_sha` on the right branch with a "
            "dirty tree is the post-OOM state, and the fast path skips the "
            "reset, so the guard below never runs and the batch builds on "
            "top of the dead turn's unrecovered work")


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


# `ultrareview.js`'s ROSTER is the only place a review lens is classified
# mutating vs. read-only (task 11, issue #265 hardening, Codex review D5).
# `EXPECTED_LENS_COUNT` mirrors the same guard `test_ultrareview_workflow.mjs`
# pins on the JS side (`rosterEntries.length >= 11`): a roster reformatted so
# the entry-line regex below stops matching some of it must fail loudly, not
# quietly check fewer lenses than exist.
ULTRAREVIEW_EXPECTED_LENS_COUNT = 11
ULTRAREVIEW_ROSTER_BLOCK_RE = re.compile(r"const ROSTER = \{(.*?)\n\}\n", re.DOTALL)
ULTRAREVIEW_ROSTER_ENTRY_RE = re.compile(r"^\s*([A-Z]):\s*\{(.*)\},?\s*$", re.MULTILINE)

# A lens reference written as its bracketed id — `(K)`, `Agent K`, `Lens K`,
# `**K**` — is deliberately matched case-SENSITIVE on the letter itself.
# Case-insensitive matching here previously turned "(a) no edits, (b) no
# commits" (an ordinary lettered list, lowercase) into a false-positive
# reference to lens A/B. A real lens id is always written uppercase.
ULTRAREVIEW_ID_REF_RES = [
    re.compile(r"\(([A-K])\)"),
    re.compile(r"\b[Aa]gent\s+([A-K])\b"),
    re.compile(r"\b[Ll]ens\s+([A-K])\b"),
    re.compile(r"\*\*([A-K])\*\*"),
]

# What immediately follows a lens reference for it to count as an actual
# classification assertion about THAT lens, as opposed to the lens merely
# being named in a sentence about something else ("the architect lens is
# dispatched during the read-only find phase" — read-only describes the
# phase, not the lens, and must not match). The optional leading filler
# absorbs a trailing "(id)" or "lens" between the name and the predicate, e.g.
# "pr-test-analyzer (G) never writes ..." or "performance-reviewer lens is
# read-only ...".
ULTRAREVIEW_TRAILING_PREDICATE_RE = re.compile(
    r"\s*(?:\([A-K]\)\s*)?(?:lens\s+)?(?:"
    r"(?:is|are)\s+(?:still\s+|currently\s+|classified\s+as\s+)?(?:read-only|mutat\w*)"
    r"|(?:never\s+)?writes?\s+to\s+the\s+(?:working\s+)?tree"
    r"|never\s+(?:writes?|edits?)\b"
    r"|runs?\s+(?:the\s+)?test\s+suite"
    r"|runs?\s+benchmarks?"
    r")",
    re.IGNORECASE,
)
# A sentence can assert a competing classification without tying it to one
# specific lens id at all — "the mutating lenses are ..." or "lenses that
# mutate: G, K" is exactly the kind of drift-prone list this task exists to
# ban, independent of whether any single id sits next to the verb.
ULTRAREVIEW_GENERIC_COMPETING_LIST_RE = re.compile(
    r"\b(?:mutating|read-only|non-mutating)\s+lenses?\b"
    r"|\blenses?\s+that\s+(?:mutate|write|edit|run\s+the\s+test\s+suite)\b",
    re.IGNORECASE,
)


def parse_ultrareview_roster(text: str) -> dict[str, dict]:
    """`{lens id: {"mutates": bool|None, "key": str|None}}`.

    `mutates` is `None` when the entry omits the field. `key` is the lens's
    own spelled-out name straight from its ROSTER entry (e.g.
    `'performance-reviewer'`), so callers never need a second, hand-maintained
    copy of the name-to-id mapping — deriving it from the same parse this
    function already does is the whole point of task 11.
    """
    block = ULTRAREVIEW_ROSTER_BLOCK_RE.search(text)
    if not block:
        return {}
    out: dict[str, dict] = {}
    for entry in ULTRAREVIEW_ROSTER_ENTRY_RE.finditer(block.group(1)):
        lens_id, entry_body = entry.group(1), entry.group(2)
        m = re.search(r"\bmutates:\s*(true|false)\b", entry_body)
        km = re.search(r"key:\s*'([^']+)'", entry_body)
        out[lens_id] = {
            "mutates": (m.group(1) == "true") if m else None,
            "key": km.group(1) if km else None,
        }
    return out


def ultrareview_name_to_id(roster: dict[str, dict]) -> dict[str, str]:
    """Spelled-out lens name -> id, derived from ROSTER's own `key` field.

    A `key` like `'code-reviewer (correctness)'` carries a parenthetical
    descriptor that nobody writing prose about the lens would reproduce
    verbatim, so it is stripped to the base name the same way a human
    shorthand would.
    """
    out: dict[str, str] = {}
    for lens_id, info in roster.items():
        key = info.get("key")
        if not key:
            continue
        base = re.sub(r"\s*\([^)]*\)\s*$", "", key).strip()
        if base:
            out[base.lower()] = lens_id
    return out


def ultrareview_paragraphs(text: str) -> list[tuple[int, str]]:
    """Blank-line-delimited blocks, each joined to a single line.

    This corpus hard-wraps prose at ~78 columns, so a sentence asserting a
    per-lens classification routinely spans two source lines (task 11 review
    finding #2). Scanning line-at-a-time missed those; joining every
    non-blank run into one string before sentence-splitting does not. The
    paragraph's first source line is kept for the error message — an
    approximation once lines are joined, but enough to locate the block.
    """
    blocks: list[tuple[int, str]] = []
    cur: list[str] = []
    cur_start = None
    for i, line in enumerate(text.splitlines(), 1):
        if line.strip() == "":
            if cur:
                blocks.append((cur_start, " ".join(cur)))
                cur = []
                cur_start = None
        else:
            if cur_start is None:
                cur_start = i
            cur.append(line)
    if cur:
        blocks.append((cur_start, " ".join(cur)))
    return blocks


def ultrareview_sentences(paragraph: str) -> list[str]:
    normalized = re.sub(r"\s+", " ", paragraph).strip()
    return re.split(r"(?<=[.!?])\s+", normalized)


def find_ultrareview_lens_refs(sentence: str, name_re: re.Pattern, name_to_id: dict[str, str]) -> list[tuple[int, int, str]]:
    """`(start, end, lens_id)` for every lens reference in `sentence`, sorted by position."""
    refs = []
    for pat in ULTRAREVIEW_ID_REF_RES:
        for m in pat.finditer(sentence):
            refs.append((m.start(), m.end(), m.group(1)))
    if name_re is not None:
        for m in name_re.finditer(sentence):
            lens_id = name_to_id.get(m.group(1).lower())
            if lens_id:
                refs.append((m.start(), m.end(), lens_id))
    refs.sort()
    return refs


def check_review_lens_mutation_classification_contract() -> None:
    """`mutates` lives in exactly one place: ultrareview.js's ROSTER.

    Task 11 (issue #265 hardening, Codex review D5): the mutating/read-only
    split for review lenses must be a machine-readable classification, not a
    sentence in a prompt — a prompt sentence can silently drift from the code
    that actually gates worktree isolation. This check has two halves: the
    ROSTER itself must classify every lens (mirrors the workflow's own
    assertion, so a parse-level regression here is caught even if
    `ultrareview.js` is edited without running its JS test), and no review
    prompt/command markdown may restate that classification in prose — even
    correctly, since a correct restatement today is exactly the copy that
    drifts tomorrow. Flag any such restatement outright rather than trying to
    prove it wrong first.

    A paragraph that itself names `ROSTER` (e.g. "see ROSTER.mutates for
    which lenses mutate") is exempted entirely: that is the sanctioned way to
    describe the policy without a second copy of it, and task 12 needs to be
    able to write exactly that sentence in these same files.
    """
    if not ULTRAREVIEW_SRC.exists():
        err(f"{ULTRAREVIEW_SRC.relative_to(ROOT)}: missing — cannot verify the "
            f"review-lens mutation classification")
        return
    roster = parse_ultrareview_roster(ULTRAREVIEW_SRC.read_text())
    if len(roster) < ULTRAREVIEW_EXPECTED_LENS_COUNT:
        err(f"{ULTRAREVIEW_SRC.relative_to(ROOT)}: only parsed {len(roster)} "
            f"ROSTER entr{'y' if len(roster) == 1 else 'ies'}, expected "
            f"{ULTRAREVIEW_EXPECTED_LENS_COUNT} — review-lens "
            f"mutation-classification check may be silently checking fewer "
            f"lenses than actually exist")
        if not roster:
            return
    missing = sorted(lens_id for lens_id, info in roster.items() if info["mutates"] is None)
    if missing:
        err(f"{ULTRAREVIEW_SRC.relative_to(ROOT)}: ROSTER entries {missing} do "
            f"not declare mutates: true|false")

    name_to_id = ultrareview_name_to_id(roster)
    name_re = (
        re.compile(r"\b(" + "|".join(re.escape(n) for n in name_to_id) + r")\b", re.IGNORECASE)
        if name_to_id else None
    )

    candidates: list[pathlib.Path] = []
    for base in (PROMPTS, COMMANDS_DIR, CODEX_PROMPTS_DIR, CODEX_COMMANDS_DIR):
        if base.is_dir():
            candidates.extend(sorted(base.glob("*.md")))
    for path in candidates:
        text = path.read_text()
        for start_line, paragraph in ultrareview_paragraphs(text):
            # Deliberately paragraph-scoped, not sentence-scoped: the
            # sanctioned "point back to ROSTER" phrasing and the sentence it
            # is clarifying are often split by a semicolon or a second
            # sentence in the same paragraph, and both must stay exempt.
            if "roster" in paragraph.lower():
                continue
            for sentence in ultrareview_sentences(paragraph):
                if ULTRAREVIEW_GENERIC_COMPETING_LIST_RE.search(sentence):
                    err(f"{path.relative_to(ROOT)}:{start_line}: describes a "
                        f"competing mutating/read-only lens list in prose — "
                        f"ROSTER.mutates in {ULTRAREVIEW_SRC.relative_to(ROOT)} "
                        f"is the only place this may be declared: "
                        f"{sentence.strip()!r}")
                flagged: set[str] = set()
                for start, end, lens_id in find_ultrareview_lens_refs(sentence, name_re, name_to_id):
                    if lens_id in flagged:
                        continue
                    trailing = sentence[end:end + 80]
                    if ULTRAREVIEW_TRAILING_PREDICATE_RE.match(trailing):
                        flagged.add(lens_id)
                        err(f"{path.relative_to(ROOT)}:{start_line}: restates "
                            f"the mutating/read-only classification for lens "
                            f"{lens_id!r} in prose — ROSTER.{lens_id}.mutates "
                            f"in {ULTRAREVIEW_SRC.relative_to(ROOT)} is the "
                            f"only place this may be declared: "
                            f"{sentence.strip()!r}")


# Contract lists for the pilot-routing checks below. They are module-level
# constants (rather than tuples inline in the functions) so the lint's own
# test suite can enumerate them entry by entry: every one of these is contract
# DATA, and a pin nobody exercises is a pin that can be deleted silently.
CODEX_PILOT_ROUTING_SNIPPETS = [
    "`PlanSynthesisPending` with normal Codex-pilot ownership",
    "`PlanClaudeFinalizePending` with normal Codex-pilot ownership",
    "`CodeReviewLocalPending` with normal Codex-pilot ownership",
    "`CodeReviewFinalPending` with normal Codex-pilot ownership",
]
COMPOSE_HANDOFF_SNIPPETS = [
    "this is a **normal compose\n      handoff**, not a dispatch failure",
    # Each bullet's `$SENDER` clause, bound to that bullet's own `$TOPIC` so
    # the pin cannot be satisfied by the identical literal in the dispatch
    # table rows.
    "`$TOPIC=final`,\n        `$ARTIFACT_REF=<drawer_id>`, and\n"
    "        `$SENDER=<collab_status.current_owner>`.",
    "`$TOPIC=final_review`, `$ARTIFACT_REF=<drawer_id>`, and\n"
    "        `$SENDER=<collab_status.current_owner>`.",
    "Do\n      not re-dispatch the compose prompt, and do not emit\n      `codex_dispatch_failed:`",
]
TASK_LIST_BRIDGE_SNIPPETS = [
    "`collab-turn-task-list.md` once (mechanical/sonnet) with\n"
    "`$SENDER=<collab_status.current_owner>`.",
    "dispatch `collab-turn-task-list.md`\n   (mechanical/sonnet) with "
    "`$SENDER=<collab_status.current_owner>`.",
    "it\n   must never be hardcoded to `\"claude\"`",
]
# The wait loop's `codex_dispatch_failed:` remedy is admissible in far fewer
# phases than "coding-active". Two independent server gates apply, and passing
# the first is not sufficient: planning phases fail `Phase::is_coding_active()`,
# while `CodeReviewLocalPending`/`CodeReviewFinalPending` pass that check but
# are still refused by `dispatch_failure_phase_admits`, which admits only
# `CodeImplementPending` (implementer=codex) and `CodeReviewFixGlobalPending`.
# An earlier revision of this contract lumped the two `CodeReview*` phases into
# the "send it" bucket on the strength of `is_coding_active()` alone; under
# `pilot == "codex"`, which routes Codex into both, that sent the dispatcher
# into a rejected send with no reachable exit.
DISPATCH_FAILURE_ADMISSIBILITY_SNIPPETS = [
    "**Dispatch-failure-admitting phases only** — `CodeImplementPending`\n"
    "        (when `implementer == \"codex\"`) and "
    "`CodeReviewFixGlobalPending`:",
    "**Every other phase** — the planning phases (`PlanParallelDrafts`,",
    "additionally requires `dispatch_failure_phase_admits`\n"
    "        (`crates/ironmem/src/collab/mod.rs`), which returns `false` "
    "for both —",
    "**Every other phase:** as in condition 6 — the planning phases are",
]
DOC_PR_BASE_SNIPPETS = [
    "does **not** require that branch to contain `base_sha`",
    "pre-range commits in the PR body",
]
DOC_PILOT_SUBMIT_SNIPPETS = [
    "$SENDER` where that template uses",
]


def live_text(path: pathlib.Path) -> str:
    """A surface's *executable* prose: file text with HTML comments removed.

    Every pin below asserts that a rule is stated where an agent will read
    it, and an agent reads the rendered body — not the commented-out
    provenance an editor parked above it. A phrase search over raw text
    cannot tell the two apart, so a substring-only pin is satisfied by its
    own epitaph: wrap the live rule in `<!-- HISTORICAL NOTE: ... -->`,
    write the opposite instruction underneath, and the lint still passes
    while the shipped instruction now says the reverse.

    That is not hypothetical. `check_reset_guards` was rewritten for exactly
    this bypass (see its docstring on issue #254), which is why it, and the
    two checks beside it, already strip comments before matching. The
    pilot/gate pins added later did not, and the single human planning gate
    — the one human checkpoint in the whole protocol — was demotable to a
    comment with CI green. Route every phrase pin through this helper so the
    lint reads what the agent reads.
    """
    return HTML_COMMENT_RE.sub("", path.read_text())


FENCE_OPEN_RE = re.compile(r"(`{3,}|~{3,})")


def command_sections(text: str, heading: str,
                     boundaries: tuple[str, ...] = ("## ",)) -> list[str]:
    """Every section whose heading line is exactly `heading`.

    `boundaries` is the set of heading prefixes that END a section. It
    defaults to `## ` alone, which is what every command-file pin wants: a
    `### ` subheading is part of the `## ` section it sits under. docs/COLLAB.md
    states the flag-parsing contract in a `### ` section, so that caller passes
    `("## ", "### ")` — otherwise the section would run on to the end of the
    file and a pin scoped to it would be satisfied by any text below it.

    Fenced code blocks are skipped when looking for boundaries. The dispatch
    loop's ```text pseudocode is full of `#` comment lines, and any line in a
    fence that happens to start with `## ` — a pseudocode comment, a shell
    snippet, a quoted markdown example — used to end the section being
    scanned. That truncation is invisible: every pin below the fence reports
    as a missing contract when nothing was deleted at all, and (worse, in the
    other direction) the tail of a section can be silently dropped out of the
    audited region while still shipping to the agent.

    Returns a list rather than the first match because a duplicated heading
    leaves a second, possibly contradictory copy of the section in the file.
    Scanning only the first copy audits the one an editor is least likely to
    have changed.
    """
    sections: list[str] = []
    body: list[str] = []
    inside = False
    fence: str | None = None
    for line in text.splitlines(keepends=True):
        stripped = line.lstrip()
        if fence is None:
            opening = FENCE_OPEN_RE.match(stripped)
            if opening:
                fence = opening.group(1)
            elif any(line.startswith(prefix) for prefix in boundaries):
                if inside:
                    sections.append("".join(body))
                    body = []
                inside = line.rstrip("\n") == heading
        elif stripped.startswith(fence[0] * len(fence)):
            fence = None
        if inside:
            body.append(line)
    if inside and body:
        sections.append("".join(body))
    return sections


def command_section(text: str, heading: str,
                    boundaries: tuple[str, ...] = ("## ",)) -> str | None:
    """The body of one section, heading line included.

    The `--pilot` parsing contract has to hold in EACH of the three subcommand
    sections, and each of them states it in its own words about its own
    positional (`<task>`, `<session_id>`, `<short-topic>`). A file-wide
    substring search cannot tell "all three sections carry the rule" from
    "`start` carries it three times", which is exactly the regression this
    guards: the flags were added to `start` first, and `join`/`review` were
    left parsing the old way for several revisions.

    A duplicated heading is reported and every copy is returned concatenated,
    so the second copy is audited too instead of sitting unchecked behind a
    `next()`-style first-match lookup. Ordering assertions built on this are
    only meaningful while there is exactly one copy, which is precisely why
    the duplicate is an error rather than a silent merge.
    """
    sections = command_sections(text, heading, boundaries)
    if len(sections) > 1:
        err(f"{heading!r} appears {len(sections)} times — every pin scoped to "
            f"this section is written for one copy, and a second copy can "
            f"contradict the first while the audit reads only one of them")
    return "".join(sections) if sections else None


# Blockquote markers are not whitespace, so a `flex()` phrase that wraps across
# two lines of a `> ` quote never matches. The unattended-successor guard —
# the one rule that keeps the single human planning gate attended — is written
# as a blockquote in both surfaces, so every multi-line pin on it has to be
# matched against a copy with the quote markers removed.
BLOCKQUOTE_MARKER_RE = re.compile(r"^[ \t]*>+[ \t]?", re.MULTILINE)


def unquoted(text: str) -> str:
    """`text` with leading blockquote markers stripped, line breaks kept."""
    return BLOCKQUOTE_MARKER_RE.sub("", text)


def markdown_tables(text: str, header: str) -> list[str]:
    """Every markdown table introduced by exactly `header`, header included.

    The Codex shim has no `## ` sections, so `command_section` cannot reach
    its phase→prompt table and a file-wide search cannot tell a routing row
    from the prose that discusses one — the file names `PlanLocked` in prose
    legitimately. A table is delimited by nothing but the run of `|` lines
    that follows its header, so that is what this returns; callers assert on
    the count so a duplicated or restructured table is reported rather than
    silently half-checked.
    """
    lines = text.splitlines()
    tables: list[str] = []
    for i, line in enumerate(lines):
        if line.strip() != header:
            continue
        body: list[str] = []
        for row in lines[i:]:
            if not row.lstrip().startswith("|"):
                break
            body.append(row)
        tables.append("\n".join(body))
    return tables


# ---- three-role (dispatcher / pilot / copilot) contracts --------------------
#
# Contract lists for the five checks below, module-level for the same reason as
# the pilot-routing lists above: every entry is contract DATA, and a pin nobody
# enumerates is a pin that can be deleted silently. Each is a plain phrase
# rather than a compiled pattern so a failure can quote the sentence the author
# has to restore; each is matched through `flex()` rather than as a literal
# because all of it is prose inside wrapped paragraphs, table rows and numbered
# lists — adding one word earlier in a sentence re-wraps it, and a literal pin
# would then report a contract as missing when it had only moved.
PILOT_FLAG_SECTION_HEADINGS = {
    "start": "## `start [--pilot=claude|codex] "
             "[--implementer=claude|codex] <task>`",
    "review": "## `review [--pilot=claude|codex] <short-topic>`",
    "join": "## `join [--pilot=claude|codex] "
            "[--implementer=claude|codex] <session_id>`",
}
# The rule every subcommand states identically: detect the flag, reject
# anything that is not `claude`/`codex`, and never paper over a malformed flag
# with the default. The no-fallback clause is the load-bearing one — a parser
# that silently defaults on `--pilot=gpt` starts a session under a role
# assignment the user did not ask for and never reports it.
PILOT_FLAG_COMMON_SNIPPETS = [
    "Detect the optional `--pilot=claude` / `--pilot=codex` flag",
    "Malformed flag input is a hard usage error",
    "do not silently fall back to the default on a malformed flag",
    "`{claude, codex}`",
]
# Per-section: the strip-before-positional-capture rule named against that
# section's own positional (a flag left in the stream lands in the task text,
# the session id, or the review topic), plus what only that section owes.
PILOT_FLAG_SECTION_SNIPPETS = {
    "start": [
        "Strip both flag tokens out of the stream before capturing the "
        "positional `<task>`",
        "an unrecognized value (`--pilot=gpt`), an empty value (`--pilot=`), "
        "the bare flag with no `=` (`--pilot`), and the same flag given more "
        "than once",
        "The identical rule, wording and accepted set `{claude, codex}` apply "
        "to `--implementer`",
    ],
    "review": [
        "strip that flag token before capturing the positional "
        "`<short-topic>`",
        "naming **both** the offending token/value **and** the accepted set "
        "`{claude, codex}`",
        # `initiator` names the DISPATCHER and is a separate axis from
        # `pilot`: `collab_start_code_review`'s `initiator` enum admits only
        # `"claude"` unconditionally, so deriving it from `--pilot` makes
        # `review --pilot=codex` fail at the server with a validation error
        # instead of starting a Codex-piloted review session.
        '`initiator` ← `"claude"`',
        'It stays `"claude"` under **every** `--pilot` value',
        "Never set `initiator` from the `--pilot` value",
        "`{repo_path, branch, base_sha, head_sha, initiator, task, pilot}`",
    ],
    "join": [
        "Strip both flag tokens out of the stream before capturing the "
        "positional `<session_id>`",
        "naming **both** the offending token/value **and** the accepted set "
        "`{claude, codex}`",
    ],
}
# ---- `--` end-of-options terminator ----------------------------------------
#
# The contract is stated once per flag-parsing subcommand — `start`, `join` and
# `review` in the Claude command file, `start` and `join` in the Codex shim,
# once for all three in docs/COLLAB.md — and it has to be the SAME contract in
# every one of them, because all three surfaces are executable agent specs and
# whichever one an agent happens to read is the parser it implements.
#
# This is pinned across all three files, not one, because single-surface drift
# is the failure mode this file exists to catch and it recurred while the
# terminator was being added: the `--` rule landed in the Claude command file
# and the Codex shim while docs/COLLAB.md still carried no flag-parsing
# contract at all, and the shim's own `join` paragraph still said flags are
# detected "anywhere in the remaining token stream" — unbounded, i.e. the
# pre-terminator parser — after the other two had been qualified. A pin that
# reads only one surface is green through all of that.
#
# What the five shared sentences buy, one hazard each:
#   1. terminator definition — without it there is no `--` at all and
#      `/collab start document how --pilot=codex behaves` silently reroutes
#      the session (and, via the implementer-inherits-pilot default, BOTH
#      roles) while dropping the token from the recorded task.
#   2. consumption      — a `--` left in the captured positional lands in the
#                         recorded task/session id/review topic.
#   3. before-first-`--`— the recognition REGION. Without it, "anywhere in the
#                         token stream" is unbounded again and rule 1 is dead
#                         letter.
#   4. anywhere-before  — the prior behaviour, preserved *inside* that region:
#                         a terminator that also made flags positional would
#                         break every existing invocation.
#   5. not-an-error     — the interaction with the malformed-flag rule. A
#                         parser that keeps `--pilot=gpt` after `--` as a
#                         literal but still raises the usage error rejects
#                         exactly the invocation the terminator exists to make
#                         work.
TERMINATOR_SHARED_SNIPPETS = [
    "**`--` ends the flags.** The first bare `--` token is the end-of-options "
    "terminator: every token after it is literal positional text, never "
    "parsed as a flag and never stripped.",
    "The `--` itself is consumed — it is not part of the captured positional.",
    "Flags are recognized only before the first `--`",
    "anywhere in the token stream before the first `--`",
    "a flag-shaped token after the first `--` is not malformed input — it is "
    "literal positional text, and it must never raise a usage error.",
]
# Per-region additions. Where a surface legitimately words the contract
# differently it is pinned in ITS wording rather than the shared pin being
# weakened to something vague enough to match every variant: the escape hatch
# names that subcommand's own positional (task / short topic / session id /
# positional text), and only the two command files restate the terminator at
# the capture site, because only they specify the capture.
#
# The escape hatch is pinned separately from the terminator definition because
# it is the half a user acts on. A file can define `--` correctly and never
# tell anyone to type it; the invocations that motivate the terminator here are
# `/collab start` tasks that are *about* the flags, and they are written by
# whoever read this section.
#
# It is pinned in ALL SIX regions, not just the four that first carried it. The
# Codex shim shipped this round with a worked `--` example under `start` and
# nothing at all under `join`, which is the same one-region-left-behind drift
# `terminator_regions()` exists to catch — an agent implementing the shim's
# `join` reads a parse that rejects "an extra positional value" and is never
# told that the consumed `--` is not one. The `join` hatch is worth stating
# even though a session id is a UUID that cannot realistically contain a
# flag-shaped token: what it buys there is the promise that `/collab join --
# <id>`, typed by a user who learned the habit from `start`, is accepted rather
# than rejected as a second positional.
TERMINATOR_REGION_SNIPPETS = {
    (".claude-plugin/commands/collab.md", "start"): [
        "**When the task text legitimately contains a flag-shaped token, put "
        "`--` before the task**",
        "`/collab start -- document how --pilot=codex behaves` records that "
        "whole sentence as the task",
        "the `--` terminator if one was given, with every token after that "
        "`--` kept verbatim",
    ],
    (".claude-plugin/commands/collab.md", "review"): [
        "**When the short topic legitimately contains a flag-shaped token, "
        "put `--` before the topic**",
        "`/collab review -- --pilot= handling` reviews that topic verbatim",
        "the `--` terminator if one was given, with every token after that "
        "`--` kept verbatim",
    ],
    (".claude-plugin/commands/collab.md", "join"): [
        "**When the session id legitimately contains a flag-shaped token, put "
        "`--` before the id**",
        "`/collab join -- <session_id>` takes the id verbatim",
        "both flags, and the `--` terminator if one was given",
    ],
    (".codex-plugin/commands/collab.md", "start"): [
        # The shim's `start` takes no `--pilot` at all, so its terminator
        # clause has to bound the REJECTION rather than the parse — otherwise
        # `/collab start -- document how --pilot=codex behaves` is a usage
        # error on this side and literal text on the other two.
        "That rejection binds only tokens before the first `--`",
        "**When the task text legitimately contains a flag-shaped token, put "
        "`--` before the task**",
        "`/collab start -- document how --pilot=codex behaves` records that "
        "whole sentence as the task rather than erroring on it",
        "the `--` terminator if one was given, with every token after that "
        "`--` kept verbatim",
    ],
    (".codex-plugin/commands/collab.md", "join"): [
        "These rules bind only tokens before the first `--`",
        "**When the session id legitimately contains a flag-shaped token, put "
        "`--` before the id**",
        # The shim's `join` is the one region whose parse rejects "an extra
        # positional value", so its worked example has to say that the
        # consumed `--` is not one — and, this side having two flags rather
        # than the Claude file's one, that neither role moves.
        "`/collab join -- <session_id>` takes the id verbatim",
        "leaves `pilot` and `implementer` both untouched",
        "both flags, and the `--` terminator if one was given, kept verbatim",
    ],
    ("docs/COLLAB.md", "`/collab` flag parsing"): [
        # The doc is the only surface that states the interaction with the
        # malformed-flag rule as its own bullet, and the only one whose usage
        # block shows `[--]` for all three subcommands at once.
        "**Malformed flag input stays a hard usage error**, unchanged by the "
        "terminator",
        "**When the positional text legitimately contains a flag-shaped "
        "token, put `--` before it.**",
        "These rules bind only tokens before the first `--`",
        "[--] <task>",
        "[--] <session_id>",
        "[--] <short-topic>",
    ],
}
DOC_FLAG_PARSING_HEADING = "### `/collab` flag parsing — `--` ends the flags"
# The Codex shim has no `## ` sections, so its two flag-parsing paragraphs are
# delimited by the sentences that open them. Each anchor must appear exactly
# once: a duplicated or vanished anchor silently re-scopes (or empties) the
# region every pin below is checked against, which is the same failure mode
# `command_section` reports a duplicated heading for.
CODEX_SHIM_FLAG_REGIONS = (
    ("start", "For `start`, select `collab-plan-draft.md`.",
     "For `join`, parse exactly one session id"),
    ("join", "For `join`, parse exactly one session id",
     "**Call `collab_status` first, before any mutation**"),
)
# The usage surfaces that advertise the terminator. These are what a user sees
# before they type anything, and they are the only place the terminator is
# discoverable without reading the spec: `[--]` missing from the hint while the
# body defines it means the feature exists and nobody knows.
#
# Pinned per REGION rather than per file, because the same usage string appears
# three times in the Claude command file (frontmatter `description`,
# `argument-hint`, and the Unknown-subcommand block) and a file-wide search
# cannot tell "all three carry `[--]`" from "the description carries it three
# times" — the identical confusion `command_section` exists to prevent for the
# subcommand sections.
TERMINATOR_USAGE_SURFACES = [
    (".claude-plugin/commands/collab.md", "fm:description", [
        "/collab start [--pilot=claude|codex] [--implementer=claude|codex] "
        "[--] <task>",
        "/collab join [--pilot=claude|codex] [--implementer=claude|codex] "
        "[--] <session_id>",
        "/collab review [--pilot=claude|codex] [--] <short-topic>",
        "(`--` ends the flags: everything after it is literal text)",
    ]),
    (".claude-plugin/commands/collab.md", "fm:argument-hint", [
        "start [--pilot=claude|codex] [--implementer=claude|codex] "
        "[--] <task>",
        "join [--pilot=claude|codex] [--implementer=claude|codex] "
        "[--] <session_id>",
        "review [--pilot=claude|codex] [--] <short-topic>",
    ]),
    (".claude-plugin/commands/collab.md", "section:## Unknown subcommand", [
        "Usage: /collab start [--pilot=claude|codex] "
        "[--implementer=claude|codex] [--] <task>",
        "[--] <session_id>",
        "[--] <short-topic>",
        "`--` ends the flags: every token after it is literal text, so put "
        "`--` before task text that contains a flag-shaped token",
    ]),
    (".codex-plugin/commands/collab.md", "fm:argument-hint", [
        # No `--pilot` on this side's `start` — see
        # CODEX_START_PILOT_REJECTION_SNIPPETS — so the hint is deliberately
        # NOT the Claude one, and pinning it verbatim is what keeps a
        # copy-paste sync from advertising a flag the shim rejects.
        "start [--implementer=claude|codex] [--] <task>",
        "join [--pilot=claude|codex] [--implementer=claude|codex] "
        "[--] <session_id>",
        "(`--` ends the flags: everything after it is literal text)",
    ]),
]
# The negative half. The pre-terminator wording said flags are detected
# "anywhere in the remaining token stream" — unbounded, with nothing to stop a
# `--pilot=` inside the positional text from being consumed as a flag — and it
# survived in the Codex shim after the other surfaces had been qualified. It is
# the wording a rewrite reaches for, because it is shorter and reads as a
# simplification rather than as a behaviour change.
#
# Matched by scanning for the unbounded SHAPE (`anywhere in the … token/argument
# stream`) and requiring the qualifier right behind it, rather than by pinning
# the one stale sentence: "remaining", "whole", "argument stream" and a bare
# "anywhere in the token stream" are the same contract and only one of them
# actually shipped. The window is generous enough for a markdown bold marker
# and a line wrap between the two halves (the docs copy wraps mid-phrase) and
# far too tight to borrow a qualifier from an unrelated sentence.
UNBOUNDED_FLAG_SCAN_RE = re.compile(
    r"anywhere\s+in\s+the\s+(?:[A-Za-z]+\s+){0,3}(?:token|argument)\s+stream",
    re.IGNORECASE)
UNBOUNDED_FLAG_QUALIFIER_RE = flex("before the first `--`")
UNBOUNDED_FLAG_QUALIFIER_WINDOW = 60
# The join-side pilot reassignment contract, per command file. Both harnesses
# carry it, mirrored around their own identity: each may hand the role away
# only while it IS the pilot, and neither may reclaim it from the other side.
# The two lists are written out rather than templated over the agent name
# precisely so a one-sided rewrite (Claude's file updated, Codex's shim left on
# the old unconditional `collab_set_pilot` call) fails here.
CLAUDE_JOIN_PILOT_SNIPPETS = [
    "Call `mcp__ironmem__collab_status` FIRST**, before any mutation",
    "Passing `--pilot` is never by itself authorization to change the pilot",
    "branch on `status.pilot` in **exactly this order**",
    "**Requested pilot matches `status.pilot`** → no-op",
    "Do not call `mcp__ironmem__collab_set_pilot`",
    '**Differs and `status.pilot == "claude"`** → authorized',
    'Call `mcp__ironmem__collab_set_pilot` with `session_id`, '
    '`agent="claude"`, and `pilot=<flag value>` — **before** any '
    '`collab_set_implementer` call',
    '**Differs and `status.pilot != "claude"`** → **fail before attempting '
    'the mutation.**',
    "**Never call `collab_set_pilot` in this branch, and never retry.**",
]
CODEX_JOIN_PILOT_SNIPPETS = [
    "Call `collab_status` first, before any mutation",
    "Passing `--pilot` is never by itself authorization to change the pilot",
    "branch on `status.pilot` in **exactly this order**",
    "**Requested pilot matches `status.pilot`** → no-op",
    "Do not call `collab_set_pilot`",
    '**Differs and `status.pilot == "codex"`** → authorized',
    'Call `collab_set_pilot` with `session_id`, `agent="codex"`, and '
    '`pilot=<flag value>` — **before** any `collab_set_implementer` call',
    '**Differs and `status.pilot != "codex"`** → **fail before attempting '
    'the mutation.**',
    "**Never call `collab_set_pilot` in this branch, and never retry.**",
]
# The human planning gate moved from the `PlanClaudeFinalizePending` dispatch
# row to the `PlanLocked` bridge, so this contract is a MOVE and both halves
# have to be asserted. The gate is dispatcher-owned: a `codex exec` one-shot
# cannot prompt a human, and under `pilot == "codex"` the finalize turn belongs
# to Codex — a gate parked on that row is unreachable exactly when it matters.
# `PlanLocked` pre-`task_list` is also the one phase where `collab_end` is
# legal, which is what makes rejection an actual exit rather than a wedge.
PLAN_LOCKED_GATE_HEADING = "## v3 Bridge: PlanLocked → CodeImplementPending"
PLAN_LOCKED_GATE_SNIPPETS = [
    "**Dispatcher-owned planning approval gate.**",
    "When `phase == PlanLocked`, `final_plan_ref` is set, and no `task_list` "
    "has been sent yet, enter harness Plan Mode and get user approval before "
    "dispatching anything",
    "This gate is the dispatcher's and no worker's",
    "surface ONLY `{drawer_id, plan_file_path, ≤3-line summary}` for approval",
    "On rejection:** do **not** send `task_list`",
    "`collab_end` is legal precisely and only at `PlanLocked` pre-`task_list`",
]
# The two ends of the ordering pin in `check_dispatcher_approval_gate_contract`.
# The gate's own heading, and the dispatch it must precede. Both are already
# required to exist (the first is in PLAN_LOCKED_GATE_SNIPPETS; the second is
# the bridge's whole point), so anchoring on them adds an ordering constraint
# without adding a new phrase anyone has to keep alive.
PLAN_LOCKED_GATE_ANCHOR = "**Dispatcher-owned planning approval gate.**"
# The step-1 dispatch, anchored on its `(mechanical/sonnet)` tier cell rather
# than on the bare verb+filename. The bare form matched the gate's OWN
# "On approval: proceed to step 1 and dispatch `collab-turn-task-list.md`"
# bullet first — a self-reference INSIDE the gate block — so the ordering
# assertion compared the gate against a line three lines below its own
# heading and passed no matter where step 1 sat. The tier cell appears on the
# real dispatch only ("It dispatches `collab-turn-task-list.md` once
# (mechanical/sonnet)" in the worker-owned preamble does not match, because
# `once` sits between the filename and the tier), and the uniqueness check
# below fails loudly if that ever stops being true.
PLAN_LOCKED_DISPATCH_ANCHOR = ("dispatch `collab-turn-task-list.md` "
                               "(mechanical/sonnet)")
# Reachability. The two anchors above prove the gate is *stated*, and stated
# before the dispatch it guards. Neither proves the loop ever arrives there.
# `PlanLocked` is in the v1 terminal set, so the dispatch-loop skeleton's
# `if session_ended or phase in terminal_set: ... exit` branch matches it —
# and under the old placement that was harmless, because the gate had already
# fired one phase earlier at `PlanClaudeFinalizePending`. Moving the gate past
# that exit makes the skeleton's ordering load-bearing: without an explicit
# pre-terminal branch, `/collab start` reaches `PlanLocked`, logs
# `t10_session_complete`, and exits with `final_plan_hash` set and the human
# never asked. Pin the branch and the reason it precedes the terminal test.
#
# The first two entries are inside the pseudocode block, where every line
# carries a leading `#`. `flex()` collapses whitespace but not comment
# markers, so each phrase must sit within a single rendered line.
PLAN_LOCKED_REACHABILITY_SNIPPETS = [
    "if phase == PlanLocked and no task_list has been sent yet:",
    "enter § v3 Bridge, step 0 (the approval gate) — do NOT exit the loop",
    "`PlanLocked` is terminal for `wait_my_turn`, not for the dispatch loop.",
    "routes there instead of exiting",
    # The other way the gate goes unattended: not by being skipped, but by
    # being handed to a process with no human on the other end. The automated
    # successor path spawns `claude -p` on an 80% context notice with no phase
    # exclusion; the gate's own premise ("a one-shot cannot prompt a human")
    # condemns that path too, and the cron fallback re-fires it every minute.
    "Never spawn an unattended successor into the planning gate.",
]
# The two ends of the reachability ORDER, inside the dispatch-loop skeleton.
# Presence of the snippets above proves the pre-terminal branch is written
# down; it proves nothing about where. Moving the whole
# `if phase == PlanLocked ...` block from above the terminal-set branch to
# below it deletes no text at all — every phrase pin stays green — and
# reinstates the exact bug the block exists to prevent, because the loop hits
# `phase in terminal_set` first and exits with `final_plan_hash` set and the
# human never asked. Both lines are already required to exist, so anchoring on
# them adds an ordering constraint without adding a phrase anyone has to keep
# alive.
PLAN_LOCKED_REACHABILITY_BRANCH_ANCHOR = (
    "if phase == PlanLocked and no task_list has been sent yet:")
DISPATCH_LOOP_TERMINAL_ANCHOR = "if session_ended or phase in terminal_set:"
# The row the gate moved OFF. Selected the same way as SENDER_DISPATCH_ROWS:
# `prefix` matches two lines (the phase-action row and the Codex dispatch
# tuning row), and `marker` picks the phase-action one.
PLAN_FINALIZE_ROW_PREFIX = "| `PlanClaudeFinalizePending` |"
PLAN_FINALIZE_ROW_COUNT = 2
PLAN_FINALIZE_ROW_MARKER = "collab-turn-plan-finalize.md"
PLAN_FINALIZE_ROW_SNIPPETS = [
    "**No human gate here — this turn is autonomous.**",
    "The single human planning gate is the dispatcher's, and it fires one "
    "phase later, at `PlanLocked` before the bridge dispatches `task_list`",
]
# The negative half of the move: the gate phrase itself, matched wherever the
# row might carry it. Case-insensitive and with `harness` optional because the
# realistic regression is the sentence being moved back onto this row, where
# it would open one — capitalized, and not necessarily naming the harness.
# Deliberately NOT shortened to "enter Plan Mode" or "get user approval" on
# their own: the row legitimately cross-references the gate it no longer owns
# ("it fires one phase later, at `PlanLocked`"), and a pin that cannot tell a
# cross-reference from a gate would fail on correct prose.
PLAN_MODE_GATE_RE = re.compile(
    r"enter\s+(?:harness\s+)?Plan\s+Mode\s+and\s+get\s+user\s+approval",
    re.IGNORECASE)
# The SHIM-side half of the same move, and the one that was pinned by nothing.
# The two halves above both live in the Claude command file; the gate they pin
# is only actually unbypassable while the Codex `/collab` shim keeps routing no
# turn at `PlanLocked` at all. That absence looks accidental from the shim
# side: nothing in its table says why the phase is skipped, and an orphaned
# `.codex-plugin/prompts/collab-task-list.md` sits on disk looking exactly like
# the missing row's other half. An editor who "completes" the pair adds
# `| \`PlanLocked\` | collab-task-list.md |`, and a Codex-terminal `join` at
# `PlanLocked` under `pilot == "codex"` then sends `task_list` from a one-shot
# `codex exec` that cannot prompt a human — no gate fires anywhere. Pinning an
# absence is the only way that edit has to argue with something.
CODEX_PHASE_TABLE_HEADER = "| Phase | Prompt |"
CODEX_UNROUTED_PHASES = ("PlanLocked",)
# The table is not the only place a route can be written. Proving `PlanLocked`
# is absent from one `| Phase | Prompt |` table says nothing about a row that
# was moved under a differently-headed table, and nothing at all about a prose
# instruction ("For `PlanLocked`, select `collab-task-list.md`") — which is how
# the rest of this shim states its non-table routing ("For `start`, select
# `collab-plan-draft.md`"). Both shapes are rejected outside the table:
#
#   - ANY `|`-leading line naming an unrouted phase, wherever it sits. The
#     shim discusses `PlanLocked` in prose legitimately and at length; it has
#     no legitimate reason to put it in a table row.
#   - Any `select <orphan prompt>` instruction that is not negated. The
#     lookbehind is what keeps the shim's own countervailing sentence
#     ("Never select `collab-task-list.md`") from tripping the pin it exists
#     to state — and that sentence is pinned positively below, because it was
#     previously guarded by nothing and is the single line telling a
#     `pilot == "codex"` one-shot not to send `task_list` unasked.
CODEX_UNROUTED_PROMPT = "collab-task-list.md"
CODEX_UNROUTED_SELECT_RE = re.compile(
    r"(?<!never )select\s+`?collab-task-list\.md`?", re.IGNORECASE)
CODEX_UNROUTED_NEGATIVE_SNIPPETS = [
    "It routes to **nothing** on this side under either pilot",
    "Never select `collab-task-list.md`.",
    "this one-shot `codex exec` cannot prompt a human",
]
# `start` on the Codex side takes no `--pilot`: `collab_start`'s pilot is set
# by whoever starts the session, and the Codex shim never starts one with a
# pilot inferred from a flag. Swallowing the token into `<task>` is the
# regression this pins — the session then starts under the default pilot with
# the flag silently embedded in the task text, so the user's stated intent is
# both ignored and un-reportable.
CODEX_START_PILOT_REJECTION_SNIPPETS = [
    "`start` takes no `--pilot` flag on this side: reject any `--pilot` token",
    "as a usage error naming the offending token",
    "Never strip it into the task text and never call `collab_start` with a "
    "pilot inferred from it",
]
# The unattended-successor guard, in both surfaces. The gate at § v3 Bridge
# step 0 is the only human checkpoint left in v1, and `claude -p` is exactly
# the one-shot process the gate's own premise says cannot take it. The
# five-phase enumeration is the load-bearing part: every phase from
# `PlanParallelDrafts` onward runs to the gate with no human checkpoint in
# between, so a successor spawned at any of them drives straight into it.
# The anti-narrowing sentence is pinned because narrowing the list back to the
# gate's own phase is the edit that looks safe and is not.
UNATTENDED_SUCCESSOR_SNIPPETS = [
    "Never spawn an unattended successor into the planning gate.",
    "The exclusion is **every v1 planning phase**, not just the gate's own",
    "if `phase` is `PlanParallelDrafts`, `PlanSynthesisPending`, "
    "`PlanCodexReviewPending`, `PlanClaudeFinalizePending`, or `PlanLocked` "
    "with no `task_list` sent",
    "use the **Interactive phases** flow below instead",
    "**Reason the list is this wide, so it cannot be narrowed without "
    "confronting it:**",
    "Dropping a phase from this list is only safe if some other human "
    "checkpoint sits ahead of the gate on that phase's path, and there is "
    "none.",
]
# The bridge's terminal-blocker rule. A `blocker:` from the task-list worker is
# not a retryable dispatch failure: the plan is immutable at `PlanLocked`, so a
# re-dispatch re-reads the same bytes for the same blocker while the loop finds
# `task_list` still unsent and re-enters the gate — an unbounded re-approval
# loop that re-prompts a human with a plan nobody can change. The shared
# sentences are pinned once; the no-re-dispatch clause is worded differently in
# each file and so is pinned per file.
BRIDGE_BLOCKER_SNIPPETS = [
    "If the worker returns `blocker:`, the bridge is over — report it and "
    "exit the loop.",
    "an unbounded re-approval loop",
]
BRIDGE_BLOCKER_COMMAND_SNIPPETS = [
    "Do not re-dispatch the worker, and do not fall back through the loop "
    "into step 0.",
    "a 16-task plan",
]
BRIDGE_BLOCKER_DOC_SNIPPETS = [
    "The orchestrator must not re-dispatch the worker or fall back through "
    "the loop into the step-0 gate.",
]
# docs/COLLAB.md's half of the dispatcher-owned gate. The command file states
# the gate as an executable step; the doc is where the *split* is defined —
# parse worker-owned, dispatch gated — and a doc that still describes the
# bridge as wholly worker-owned is the argument a future editor would use for
# deleting step 0 from the command file.
DOC_BRIDGE_GATE_SNIPPETS = [
    "The bridge's **parse** is worker-owned; its **dispatch** is gated.",
    "But before it dispatches anything, it takes the **dispatcher-owned "
    "planning approval gate** at step 0",
    "it enters Plan Mode and gets user approval, surfacing only "
    "`{drawer_id, plan_file_path, ≤3-line summary}`",
    "This gate is the dispatcher's and no worker's",
    "On rejection it does not send `task_list` and offers `collab_end` "
    "instead.",
]
# `collab_set_pilot` was removed from the unattended successor's permission
# allowlist: the successor is spawned as a bare `join ... with token ...`,
# which carries no `--pilot`, so no successor path reaches the call — and
# granting it would hand an unattended process the one tool that can reassign
# who leads planning. The pin has to be scoped to the allowlist BULLETS: the
# identifier legitimately appears immediately below them (the paragraph saying
# why it is absent), in the generation-lease claim list, and in the `join`
# authorization contract, so a file-wide negative would be wrong in three
# places at once.
PERMISSION_ALLOWLIST_HEADING = (
    "**Permission allowlist for unattended successor operation:**")
PERMISSION_ALLOWLIST_REQUIRED_ENTRY = "mcp__ironmem__collab_send"
PERMISSION_ALLOWLIST_FORBIDDEN_ENTRY = "collab_set_pilot"
PERMISSION_ALLOWLIST_RATIONALE_SNIPPETS = [
    "`mcp__ironmem__collab_set_pilot` is deliberately **not** on this list",
    "No successor path reaches it",
]
# `pilot`/`copilot` are resolved once per iteration and read from those
# bindings everywhere else. Two call sites that each work out who leads —
# from a phase name, a template filename, or a value remembered from the
# previous iteration — is split-brain routing: the loop dispatches a local
# Claude worker and a `codex exec` for the same turn.
SINGLE_PILOT_RESOLUTION_HEADING = "## Dispatch Loop Structure"
SINGLE_PILOT_RESOLUTION_SNIPPETS = [
    "status = collab_status(session_id)",
    "pilot = status.pilot",
    'copilot = counterpart(pilot)  # "codex" if pilot == "claude", '
    'else "claude"',
    "**`pilot`/`copilot` are bound once, at the top of the iteration, from "
    "the same `collab_status` read that yields `phase` and `current_owner`.**",
    "No call site may re-derive role identity from a phase name, a prompt "
    "filename, or a value remembered from a prior iteration",
    "split-brain routing",
]
# docs/COLLAB.md is the only place the three roles are DEFINED; the command
# files use the vocabulary without restating it. Keeping the doc pinned here
# is what keeps prose and command file in sync by lint rather than by
# discipline — every one of these six was written to answer a question the
# command file deliberately does not answer.
DOC_PILOT_ROLE_CONTRACTS = {
    "the three role definitions (§ Runtime Model → Roles)": [
        "- **dispatcher** — runs the control loop shown above and is the only "
        "role that talks to the human",
        "always Claude in this codebase",
        "- **pilot** — the `collab_sessions.pilot` field, per-session, "
        "default `claude`",
        "- **copilot** — `counterpart(pilot)`, derived on the fly and never "
        "stored",
        "**pilot is not the same role as dispatcher.**",
    ],
    "the `pilot` Session State row": [
        "| `pilot` | Which agent leads v1 planning and the v3 review-audit "
        "turns",
        "Rebindable via `collab_set_pilot`",
        "only by the agent that is currently the pilot",
        # This row used to end the sentence above with "— a copilot can never
        # promote itself". That absolute claim contradicts the
        # `collab_set_pilot` tool section in this same file, which states the
        # check "does **not** defeat a caller willing to misrepresent its own
        # identity, since `agent` is caller-asserted rather than
        # authenticated". Pinning the caveat instead of the absolute claim
        # keeps the restriction stated while making it impossible for the row
        # to drift back to promising a guarantee the check does not give.
        "That check is caller-*asserted*, not authenticated",
    ],
    # `collab_set_implementer` gained the same current-pilot-only caller
    # restriction as `collab_set_pilot` (#264). Pinned here so the summary row
    # — the first place a reader looks for field semantics — can never drift
    # back to describing the tool as callable by either agent.
    "the `implementer` Session State row": [
        "| `implementer` | Which agent runs the v3 batch implementation phase",
        "rebindable with `collab_set_implementer`",
        "and only by the agent that is currently the pilot — the same "
        "caller-asserted restriction as `collab_set_pilot`, above",
    ],
    "the `collab_set_pilot` tool section": [
        "### `collab_set_pilot`",
        "Only the session's *current* pilot may call this",
        "Legal only in `PlanParallelDrafts`",
    ],
    "the wire-compat note": [
        "**Wire-compat note:**",
        '`Phase::PlanCopilotReviewPending` still serializes as '
        '`"PlanCodexReviewPending"`',
        '`Phase::PlanFinalizePending` still serializes as '
        '`"PlanClaudeFinalizePending"`',
    ],
    "the Codex-terminal non-goal": [
        "### Codex-terminal-led sessions are a non-goal",
        "Generalizing the Codex-side prompt into a symmetric long-running "
        "dispatcher is intentionally out of scope",
    ],
    "the N-party amendment": [
        "**Pilot-configurability is not a step toward an N-party protocol.**",
        "does not open `Agent` to a third variant or generalize `counterpart` "
        "to an N-party lookup",
    ],
    "the schema v19 migration note": [
        "### Migration note: `pilot` column (schema v19)",
        "`crates/ironmem/migrations/019_collab_pilot.sql`",
    ],
    # The prose half of the shim-side pin below. The prompt inventory used to
    # describe `collab-task-list.md` as "Codex's `PlanLocked` task-list bridge
    # turn (pilot)" — a description of routing that has never existed, and the
    # single best argument a future editor could have for adding the row that
    # bypasses the gate. Saying why the file is unrouted is what stops the
    # absence from reading as an unfinished job.
    "the unrouted `collab-task-list.md` note": [
        "**installed but deliberately unrouted.**",
        "the Codex shim's phase→prompt table carries **no `PlanLocked` row** "
        "and must never grow one",
        "Routing `PlanLocked` through the shim would bypass that gate",
    ],
}


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
    # The docs mirror of the same contract. These two used to be checked
    # inside `check_codex_pilot_compose_handoff_contract` under a
    # "pilot-submit routing" label, which sent anyone grepping the diagnostic
    # to the wrong function; they are PR-base resolution snippets and belong
    # here, under this function's name.
    doc_text = DOC.read_text()
    for snippet in DOC_PR_BASE_SNIPPETS:
        if snippet not in doc_text:
            err(f"docs/COLLAB.md: missing PR-base resolution contract "
                f"{snippet!r}")


def check_pr_create_failed_doc_pointer_contract() -> None:
    """The shrunk `pr_create_failed:` comment must still resolve, both ways.

    `check_collab_turn_templates.py`'s `DOCUMENTED_TERMINAL_PREFIXES` comment
    deliberately doesn't restate why `pr_create_failed:` stays Terminal — it
    points at a heading in docs/COLLAB.md instead, so the rationale lives in
    exactly one place. A pointer like that can break from either end:
    docs/COLLAB.md could drop or rename the heading, or this file's comment
    could drift to naming a different string. Pinning only the doc side
    would miss the second case entirely — a comment edited to reference a
    stale heading would still pass. Both sides are checked here against the
    single `PR_CREATE_FAILED_DOC_HEADING` constant, so a mismatch on either
    end fails loudly instead of leaving a dead pointer.
    """
    # This file's own pointer comment sits immediately above
    # `DOCUMENTED_TERMINAL_PREFIXES = {`. Read this script's actual source
    # (not `ROOT`-relative — the checker's own location is fixed regardless
    # of `COLLAB_LINT_ROOT`) and confirm the comment window still quotes the
    # anchor byte-identically.
    self_text = pathlib.Path(__file__).resolve().read_text()
    anchor_def = 'PR_CREATE_FAILED_DOC_HEADING = "'
    prefixes_def = "DOCUMENTED_TERMINAL_PREFIXES = {"
    anchor_idx = self_text.find(anchor_def)
    prefixes_idx = self_text.find(prefixes_def)
    if anchor_idx == -1 or prefixes_idx == -1 or prefixes_idx <= anchor_idx:
        err("scripts/check_collab_turn_templates.py: could not locate the "
            "PR_CREATE_FAILED_DOC_HEADING constant and the "
            "DOCUMENTED_TERMINAL_PREFIXES comment above it to cross-check")
        return
    comment_window = self_text[anchor_idx:prefixes_idx]
    if comment_window.count(PR_CREATE_FAILED_DOC_HEADING) < 2:
        err("scripts/check_collab_turn_templates.py: the "
            "DOCUMENTED_TERMINAL_PREFIXES pointer comment no longer quotes "
            f"{PR_CREATE_FAILED_DOC_HEADING!r} byte-identically — the "
            "comment and PR_CREATE_FAILED_DOC_HEADING have drifted apart")

    # docs/COLLAB.md side: the anchor must appear as the literal `####`
    # heading, not merely as a passing mention in prose elsewhere in the
    # file — a reorg that folds the section into inline prose should fail
    # this, not pass it.
    doc_text = DOC.read_text()
    if f"#### {PR_CREATE_FAILED_DOC_HEADING}" not in doc_text:
        err(f"docs/COLLAB.md: missing the {PR_CREATE_FAILED_DOC_HEADING!r} "
            "`####` heading that scripts/check_collab_turn_templates.py's "
            "DOCUMENTED_TERMINAL_PREFIXES comment points at")


# The two collab.md dispatch rows that hand off to collab-turn-submit.md
# with a `$SENDER` substitution.
#
# `prefix` does NOT uniquely identify the row: collab.md has TWO lines
# starting with each of these prefixes — the phase-action table row, and the
# Codex dispatch tuning matrix row for the same phase. A `next(...)` lookup
# silently audited the first and left the second entirely unchecked. What is
# guaranteed instead, and asserted below, is: exactly `row_count` lines start
# with `prefix`, and exactly ONE of them also names `marker` (the submit
# worker) — that one is the dispatch row this contract is about. Both numbers
# are pinned so that adding, removing or restructuring a row fails loudly here
# rather than quietly re-pointing the audit at a different line.
#
# `recovery_snippets` are phase-SPECIFIC on purpose. A bare
# `recovery[-\s]owner` search cannot tell "names the recovery case" from
# "mentions the phrase while saying it does not apply": the
# PlanClaudeFinalizePending row legitimately says the substitution does NOT
# apply (PlanLocked-side phases are not `is_coding_active()`), and rewriting
# the CodeReviewFinalPending row to that same negation used to pass — while
# `is_coding_active()` makes the recovery case genuinely live for that phase.
# So each phase pins the exact form its own prose must take, and
# CodeReviewFinalPending additionally forbids the negation. Literal snippets
# also make the check occurrence-count-independent: a second "recovery owner"
# mention elsewhere in the row can neither satisfy nor defeat it.
SENDER_DISPATCH_ROWS = {
    "PlanClaudeFinalizePending": {
        "prefix": "| `PlanClaudeFinalizePending` |",
        "row_count": 2,
        "marker": "collab-turn-submit.md",
        "recovery_snippets": [
            "recovery-owner substitution",
            "does **not** apply to this phase",
        ],
        "forbidden_snippets": [],
    },
    "CodeReviewFinalPending": {
        "prefix": "| `CodeReviewFinalPending` |",
        "row_count": 2,
        "marker": "collab-turn-submit.md",
        "recovery_snippets": [
            "may instead be the recovery owner per the recovery override",
            "is a coding-active phase, so this substitution is live here",
        ],
        "forbidden_snippets": [
            "does **not** apply to this phase",
        ],
    },
}
# A pilot-only derivation of $SENDER — the exact regression this guards
# against: rewriting `$SENDER=<collab_status.current_owner>` back to a
# form that assigns $SENDER directly from `pilot`, bypassing the
# recovery-owner substitution. Deliberately anchored on `$SENDER\s*=` so
# it does NOT flag the row's own (required) prose stating the normal
# invariant `current_owner == pilot`, which never puts `pilot` on the
# right-hand side of a `$SENDER` assignment.
PILOT_ONLY_SENDER_RE = re.compile(r"\$SENDER\s*=\s*<?\s*(?:collab_status\.)?pilot\b")


def check_sender_dispatch_contract() -> None:
    """Both submit-dispatch rows must derive `$SENDER` from `current_owner`.

    Regression guard for the status-to-template routing decision itself
    (as opposed to the submit template's own contract, pinned separately):
    the orchestrator rows in collab.md for `PlanClaudeFinalizePending` and
    `CodeReviewFinalPending` must (1) name `collab_status.current_owner` as
    the `$SENDER` substitution source, (2) state the recovery-owner case in
    the exact form that phase requires — live for `CodeReviewFinalPending`,
    explicitly inapplicable for `PlanClaudeFinalizePending` — and (3) never
    state a pilot-only derivation of `$SENDER`. If either row regresses
    back to deriving `$SENDER` straight from `pilot`, the recovery-owner
    substitution silently stops applying and pilot-capable submits break
    again under recovery.

    The pilot-only derivation is additionally rejected file-wide: the two
    audited rows are not the only places collab.md substitutes `$SENDER`
    (the compose-handoff bullets and the PlanLocked bridge do too), and a
    pilot-only derivation is wrong in every one of them.
    """
    raw = COMMAND.read_text()
    # Row selection and the required pins read the *live* body: a table row
    # parked in an HTML comment is invisible to the agent, so counting it
    # toward `row_count` audits a row that does not ship. The forbidden
    # pilot-only derivation below stays on the raw text — flagging it even
    # inside a comment is strictly stricter, and the raw text is also what
    # keeps its line numbers pointing at the real file.
    text = live_text(COMMAND)
    lines = text.splitlines()
    for phase, spec in SENDER_DISPATCH_ROWS.items():
        prefix = spec["prefix"]
        matching = [l for l in lines if l.startswith(prefix)]
        if len(matching) != spec["row_count"]:
            err(f"collab.md: expected exactly {spec['row_count']} lines "
                f"starting with {prefix!r}, found {len(matching)} — the "
                f"{phase} dispatch-row audit selects one of them by its "
                f"{spec['marker']!r} cell, so an added, removed or "
                f"restructured row means the audit is reading a different "
                f"table than the one it was written for")
        rows = [l for l in matching if spec["marker"] in l]
        if len(rows) != 1:
            err(f"collab.md: expected exactly one {phase} row naming "
                f"{spec['marker']!r} (the submit-dispatch row), found "
                f"{len(rows)}")
            continue
        row = rows[0]
        if "$SENDER=<collab_status.current_owner>" not in row:
            err(f"collab.md: {phase} row must derive $SENDER from "
                f"`$SENDER=<collab_status.current_owner>` (current_owner "
                f"read from collab_status)")
        for snippet in spec["recovery_snippets"]:
            if snippet not in row:
                err(f"collab.md: {phase} row must name the recovery-owner "
                    f"case in the form this phase requires — missing "
                    f"{snippet!r}")
        for snippet in spec["forbidden_snippets"]:
            if snippet in row:
                err(f"collab.md: {phase} row must not disclaim the "
                    f"recovery-owner substitution ({snippet!r}) — this phase "
                    f"is coding-active, so the substitution is live and "
                    f"$SENDER may legitimately be the recovery owner")
        if PILOT_ONLY_SENDER_RE.search(row):
            err(f"collab.md: {phase} row must not derive $SENDER directly "
                f"from pilot — $SENDER must come from current_owner")
    for line_no, line in enumerate(raw.splitlines(), 1):
        if PILOT_ONLY_SENDER_RE.search(line):
            err(f"collab.md:{line_no}: $SENDER must never be derived directly "
                f"from `pilot` — every substitution site reads "
                f"`collab_status.current_owner`, so that the recovery owner "
                f"is honoured wherever the substitution is live")


def check_codex_pilot_routing_contract() -> None:
    """Codex must select a prompt for all four Codex-pilot-owned phases.

    Two of these are compose turns that stage a drawer and send nothing
    (`PlanClaudeFinalizePending`, `CodeReviewFinalPending`); the other two
    are SENDING turns that advance the phase themselves
    (`PlanSynthesisPending`, `CodeReviewLocalPending`). What they share is
    that under `pilot == "codex"` Codex owns them, so a missing selector row
    leaves the turn undispatchable.
    """
    if not CODEX_COMMAND.exists():
        err(".codex-plugin/commands/collab.md: missing Codex-pilot routing contract")
        return
    codex_text = live_text(CODEX_COMMAND)
    for snippet in CODEX_PILOT_ROUTING_SNIPPETS:
        if snippet not in codex_text:
            err(".codex-plugin/commands/collab.md: missing Codex-pilot "
                f"routing contract {snippet!r}")


def check_codex_pilot_compose_handoff_contract() -> None:
    """Keep normal Codex-pilot compose turns connected to Claude submission.

    The two compose prompts intentionally stage an immutable drawer and exit
    without a collab send, so their phase does not advance. If Claude's
    no-phase-advance handoff drops this special path, the dispatcher reports
    a false `codex_dispatch_failed:` and the sender-parameterization fix is
    unreachable under `pilot=codex`.

    Each of the two handoff bullets pins its `$SENDER` substitution as part
    of a snippet spanning that bullet's own `$TOPIC`. The bare literal
    `` `$SENDER=<collab_status.current_owner>` `` occurs six times in
    collab.md, so pinning it alone proved nothing about these bullets:
    rewriting either of them to `$SENDER=<pilot>`, to `$SENDER=claude`, or
    deleting the clause outright all left this check green on the strength of
    the dispatch-table rows.
    """
    command_text = live_text(COMMAND)
    for snippet in COMPOSE_HANDOFF_SNIPPETS:
        if snippet not in command_text:
            err(".claude-plugin/commands/collab.md: missing Codex-pilot "
                f"compose handoff contract {snippet!r}")


def check_task_list_bridge_sender_contract() -> None:
    """The PlanLocked bridge must pass `$SENDER` from `current_owner`.

    `PublishFinal` does not reassign ownership, so under `pilot == "codex"`
    `PlanLocked` is entered with `current_owner == codex` while
    `SubmitTaskList` requires `pilot(session)`. The bridge dispatches the
    Claude-side `collab-turn-task-list.md` worker, which must therefore send
    as the pilot, not as itself. The bridge is a prose paragraph plus a
    numbered list item — not a table row — so the dispatch-row helper above
    does not reach it.
    """
    command_text = live_text(COMMAND)
    for snippet in TASK_LIST_BRIDGE_SNIPPETS:
        if snippet not in command_text:
            err(".claude-plugin/commands/collab.md: missing PlanLocked "
                f"task-list bridge sender contract {snippet!r}")


def check_planning_dispatch_failure_contract() -> None:
    """A Codex dispatch failure in a planning phase must not send a report.

    `mod.rs` gates the whole `FailureReport` arm on
    `Phase::is_coding_active()`, and `phase.rs` limits that to the four
    `Code*` phases. Routing Codex into `PlanSynthesisPending` /
    `PlanClaudeFinalizePending` made the wait loop's `codex_dispatch_failed:`
    remedy reachable in phases where the server rejects it as `WrongPhase` —
    and the stated exits via conditions 2 and 3 are unreachable there, so the
    dispatcher would loop on a rejected send instead of surfacing the stall.
    """
    command_text = live_text(COMMAND)
    for snippet in DISPATCH_FAILURE_ADMISSIBILITY_SNIPPETS:
        if snippet not in command_text:
            err(".claude-plugin/commands/collab.md: missing dispatch-failure "
                f"admissibility contract {snippet!r}")


def check_pilot_submit_doc_contract() -> None:
    """docs/COLLAB.md must document `$SENDER` as a worker placeholder."""
    doc_text = live_text(DOC)
    for snippet in DOC_PILOT_SUBMIT_SNIPPETS:
        if snippet not in doc_text:
            err("docs/COLLAB.md: missing pilot-submit routing contract "
                f"{snippet!r}")


def check_pilot_flag_parsing_contract() -> None:
    """`start`, `join` and `review` must all parse `--pilot` the same way.

    Each of the three subcommand sections must accept
    `--pilot=claude|codex`, strip the flag tokens out of the stream *before*
    capturing its positional argument, and reject any other value outright
    instead of falling back to the default. `review` additionally keeps
    sending `initiator="claude"` under every `--pilot` value: `initiator`
    names the dispatcher, and `collab_start_code_review` admits no other
    value for it, so a `--pilot`-derived `initiator` turns
    `review --pilot=codex` into a server-side validation failure.
    """
    text = live_text(COMMAND)
    for name, heading in PILOT_FLAG_SECTION_HEADINGS.items():
        section = command_section(text, heading)
        if section is None:
            err(f".claude-plugin/commands/collab.md: missing `{name}` "
                f"subcommand section {heading!r} — the `--pilot` parsing "
                f"contract is pinned per section, so a renamed heading drops "
                f"that section's pins silently")
            continue
        for phrase in (PILOT_FLAG_COMMON_SNIPPETS
                       + PILOT_FLAG_SECTION_SNIPPETS[name]):
            if not flex(phrase).search(section):
                err(f".claude-plugin/commands/collab.md: `{name}` section is "
                    f"missing pilot-flag parsing contract {phrase!r}")


def terminator_regions() -> list[tuple[str, str, str, str]]:
    """Regions that must state the `--` terminator: (rel, key, label, text).

    A region is one subcommand's flag-parsing prose. Regions are extracted
    per surface rather than searched file-wide for the reason
    `command_section` exists: a file-wide match cannot tell "`start`, `join`
    and `review` each carry the rule" from "`start` carries it three times",
    and adding the flags to `start` first and leaving the other two on the old
    parse is the regression that has now happened twice on this contract.

    A missing region is reported here and skipped, rather than silently
    contributing no pins — an empty region satisfies nothing, but only if
    someone is told it was empty.
    """
    regions: list[tuple[str, str, str, str]] = []
    command_text = live_text(COMMAND)
    for name in ("start", "review", "join"):
        section = command_section(command_text,
                                  PILOT_FLAG_SECTION_HEADINGS[name])
        if section is None:
            err(f".claude-plugin/commands/collab.md: missing `{name}` "
                f"subcommand section — the `--` end-of-options terminator "
                f"contract is pinned per subcommand, so a renamed heading "
                f"drops that subcommand's terminator pins silently")
            continue
        regions.append((".claude-plugin/commands/collab.md", name,
                        f"`{name}`", section))

    rel = ".codex-plugin/commands/collab.md"
    if not CODEX_COMMAND.exists():
        err(f"{rel}: missing Codex slash command, so its `--` end-of-options "
            f"terminator contract is unpinned")
    else:
        shim = live_text(CODEX_COMMAND)
        for name, opening, closing in CODEX_SHIM_FLAG_REGIONS:
            counts = [(anchor, len(flex(anchor).findall(shim)))
                      for anchor in (opening, closing)]
            bad = [(anchor, n) for anchor, n in counts if n != 1]
            if bad:
                for anchor, n in bad:
                    err(f"{rel}: the `{name}` flag-parsing region is "
                        f"delimited by {anchor!r}, which appears {n} times "
                        f"(expected exactly 1) — the region every `--` "
                        f"terminator pin is checked against cannot be "
                        f"located")
                continue
            start = flex(opening).search(shim).start()
            end = flex(closing).search(shim).start()
            if end <= start:
                err(f"{rel}: the `{name}` flag-parsing region's closing "
                    f"anchor {closing!r} precedes its opening anchor "
                    f"{opening!r}")
                continue
            regions.append((rel, name, f"`{name}`", shim[start:end]))

    section = command_section(live_text(DOC), DOC_FLAG_PARSING_HEADING,
                              ("## ", "### "))
    if section is None:
        err(f"docs/COLLAB.md: missing {DOC_FLAG_PARSING_HEADING!r} — the "
            f"spec carried no flag-parsing contract at all until the "
            f"terminator was added, which is how the two command files came "
            f"to disagree with nothing to arbitrate between them")
    else:
        regions.append(("docs/COLLAB.md", "`/collab` flag parsing",
                        "§ `/collab` flag parsing", section))
    return regions


def check_end_of_options_terminator_contract() -> None:
    """`--` must end the flags identically in all three surfaces.

    Five sentences have to hold in every flag-parsing region (see
    TERMINATOR_SHARED_SNIPPETS for what each one buys), plus that region's own
    escape hatch and capture rule. All three files are checked because the
    contract is only real if all three hold it: they are executable agent
    specs, and an agent implements whichever one it reads.
    """
    for rel, key, label, text in terminator_regions():
        phrases = TERMINATOR_SHARED_SNIPPETS + TERMINATOR_REGION_SNIPPETS.get(
            (rel, key), [])
        for phrase in phrases:
            if not flex(phrase).search(text):
                err(f"{rel}: {label} is missing `--` end-of-options "
                    f"terminator contract {phrase!r}")


def check_terminator_usage_surfaces() -> None:
    """`[--]` must appear in every usage string that advertises the flags."""
    for rel, spec, phrases in TERMINATOR_USAGE_SURFACES:
        path = ROOT / rel
        if not path.exists():
            err(f"{rel}: missing usage surface for the `--` terminator")
            continue
        kind, _, key = spec.partition(":")
        if kind == "fm":
            fm = parse_frontmatter(path.read_text())
            region = None if fm is None else fm.get(key)
            label = f"frontmatter `{key}`"
        else:
            region = command_section(live_text(path), key)
            label = f"§ `{key.removeprefix('## ')}`"
        if region is None:
            err(f"{rel}: {label} is missing entirely — the `[--]` usage "
                f"contract is pinned there, and a usage string that never "
                f"shows `[--]` leaves the terminator undiscoverable")
            continue
        for phrase in phrases:
            if not flex(phrase).search(region):
                err(f"{rel}: {label} is missing `--` terminator usage "
                    f"{phrase!r}")


def check_no_unbounded_flag_scan() -> None:
    """No surface may describe flag detection over an unterminated stream.

    The qualifier is the whole terminator: "anywhere in the token stream" with
    no `before the first `--`` behind it is the pre-terminator parser, in
    which a `--pilot=` inside the positional text is consumed as a real flag no
    matter where the user put `--`. That exact sentence outlived the fix in the
    Codex shim while the other two surfaces were already qualified, so this is
    checked in all three files.
    """
    for path, rel in ((COMMAND, ".claude-plugin/commands/collab.md"),
                      (CODEX_COMMAND, ".codex-plugin/commands/collab.md"),
                      (DOC, "docs/COLLAB.md")):
        if not path.exists():
            continue
        text = live_text(path)
        for match in UNBOUNDED_FLAG_SCAN_RE.finditer(text):
            window = text[match.end():
                          match.end() + UNBOUNDED_FLAG_QUALIFIER_WINDOW]
            if not UNBOUNDED_FLAG_QUALIFIER_RE.search(window):
                line_no = text.count("\n", 0, match.start()) + 1
                err(f"{rel}:{line_no}: flag detection is described over an "
                    f"unbounded stream ({match.group(0)!r}) with no "
                    f"\"before the first `--`\" qualifier behind it — that is "
                    f"the pre-terminator parser, which consumes a "
                    f"flag-shaped token in the positional text as a real flag "
                    f"however the user quoted it")


def check_pilot_join_authorization_contract() -> None:
    """Both command files must carry the role-aware `join --pilot` contract.

    `collab_set_pilot` is caller-restricted server-side: only the session's
    current pilot may reassign the role, checked before any phase check. So
    the flag states an intent and `status.pilot` decides whether that intent
    is even attemptable — which is why each file must read `collab_status`
    first and branch three ways: no-op when the requested pilot already
    matches, mutate only when the caller IS the current pilot, and fail
    *before* calling when it is not. The third branch is the one that has to
    be prose: attempting the call and handling the rejection looks similar
    but reports a server error where the user needs to be told that
    reclaiming the role requires a join from the other side.

    Both files are checked because the contract is only real if both sides
    hold it: the Codex shim is a much shorter file with no `##` sections and
    is the easier one to leave behind on a rewrite, and a shim that calls
    `collab_set_pilot` whenever the flag is present — the shape this rules
    out — turns every copilot-side re-join into a server rejection, in a
    one-shot process with nowhere to report it.
    """
    surfaces = [
        (COMMAND, ".claude-plugin/commands/collab.md",
         PILOT_FLAG_SECTION_HEADINGS["join"], CLAUDE_JOIN_PILOT_SNIPPETS),
        (CODEX_COMMAND, ".codex-plugin/commands/collab.md",
         None, CODEX_JOIN_PILOT_SNIPPETS),
    ]
    for path, rel, heading, snippets in surfaces:
        if not path.exists():
            err(f"{rel}: missing join pilot-authorization contract")
            continue
        text = live_text(path)
        if heading is not None:
            section = command_section(text, heading)
            if section is None:
                err(f"{rel}: missing `join` subcommand section {heading!r}")
                continue
            text = section
        for phrase in snippets:
            if not flex(phrase).search(text):
                err(f"{rel}: missing join pilot-authorization contract "
                    f"{phrase!r}")


def check_dispatcher_approval_gate_contract() -> None:
    """The human planning gate belongs to the dispatcher, at `PlanLocked`.

    This contract is a MOVE, so both halves are asserted: the gate must be
    stated in the `PlanLocked` bridge, and it must be gone from the
    `PlanClaudeFinalizePending` dispatch row it moved off. A gate left on
    that row is unreachable under `pilot == "codex"` — the finalize turn is
    Codex's there, and a `codex exec` one-shot cannot prompt a human — while
    `PlanLocked` pre-`task_list` is the one phase where `collab_end` is
    legal, which is what lets rejection abandon the session cleanly instead
    of wedging it.

    Both halves here are about the Claude command file. The shim side of the
    same contract — the Codex `/collab` table routing nothing at
    `PlanLocked`, which is what keeps the bridge the dispatcher's under
    either pilot — is pinned by
    `check_codex_shim_unrouted_phases_contract` below, under its own name so
    that failure says which file has to change.
    """
    text = live_text(COMMAND)
    bridge = command_section(text, PLAN_LOCKED_GATE_HEADING)
    if bridge is None:
        err(f".claude-plugin/commands/collab.md: missing bridge section "
            f"{PLAN_LOCKED_GATE_HEADING!r}, which owns the dispatcher's "
            f"planning approval gate")
    else:
        for phrase in PLAN_LOCKED_GATE_SNIPPETS:
            if not flex(phrase).search(bridge):
                err(f".claude-plugin/commands/collab.md: v3 bridge is missing "
                    f"dispatcher approval-gate contract {phrase!r}")
        # Presence is not enough: a gate is only a gate while it precedes the
        # thing it gates. Every phrase above can be satisfied by a paragraph
        # sitting *below* the `collab-turn-task-list.md` dispatch, which reads
        # as an explanatory footnote and executes as no gate at all — the
        # bridge would send `task_list` and only then ask. Pin the order, the
        # same way `check_reset_guards` pins its precondition against the
        # first `git reset --hard` rather than merely requiring both strings.
        gate = flex(PLAN_LOCKED_GATE_ANCHOR).search(bridge)
        dispatches = list(flex(PLAN_LOCKED_DISPATCH_ANCHOR).finditer(bridge))
        if len(dispatches) > 1:
            err(f".claude-plugin/commands/collab.md: expected exactly one "
                f"{PLAN_LOCKED_DISPATCH_ANCHOR!r} in the v3 bridge — the "
                f"approval gate is ordered against it, and with more than one "
                f"the assertion silently re-anchors on whichever comes first, "
                f"found {len(dispatches)}")
        dispatch = dispatches[0] if dispatches else None
        if gate and dispatch and gate.start() > dispatch.start():
            err(f".claude-plugin/commands/collab.md: the dispatcher approval "
                f"gate ({PLAN_LOCKED_GATE_ANCHOR!r}) must appear BEFORE the "
                f"bridge dispatches {PLAN_LOCKED_DISPATCH_ANCHOR!r}. A gate "
                f"stated after the dispatch it guards is documentation, not "
                f"a gate: the bridge would send `task_list` from the "
                f"already-immutable plan and only then ask a human, which is "
                f"the approval bypass this relocation exists to close")
        elif not dispatch:
            err(f".claude-plugin/commands/collab.md: v3 bridge no longer "
                f"names {PLAN_LOCKED_DISPATCH_ANCHOR!r}, so the approval "
                f"gate's ordering can no longer be checked against the "
                f"dispatch it guards — re-anchor PLAN_LOCKED_DISPATCH_ANCHOR")
    # Reachability: the loop must route to the gate instead of exiting at it.
    for phrase in PLAN_LOCKED_REACHABILITY_SNIPPETS:
        if not flex(phrase).search(text):
            err(f".claude-plugin/commands/collab.md: the dispatch loop must "
                f"route `PlanLocked` pre-`task_list` into the approval gate "
                f"rather than exiting on the v1 terminal set — missing "
                f"{phrase!r}. Without it the skeleton's terminal-set branch "
                f"matches `PlanLocked` first and `/collab start` ends the "
                f"session with `final_plan_hash` set and the plan never "
                f"approved, making the gate unreachable in the one flow that "
                f"always passes through it")
    # ...and the ORDER of that branch against the exit it must pre-empt.
    # Presence is satisfied by the same block moved below the terminal-set
    # test, which deletes nothing, keeps every phrase pin green, and restores
    # the bug verbatim: `PlanLocked` is in the v1 terminal set, so the loop
    # matches the exit first and never reaches the gate.
    loop = command_section(text, SINGLE_PILOT_RESOLUTION_HEADING)
    if loop is None:
        err(f".claude-plugin/commands/collab.md: missing "
            f"{SINGLE_PILOT_RESOLUTION_HEADING!r} section, so the approval "
            f"gate's reachability cannot be ordered against the dispatch "
            f"loop's terminal-set exit")
    else:
        branch = flex(PLAN_LOCKED_REACHABILITY_BRANCH_ANCHOR).search(loop)
        terminal = flex(DISPATCH_LOOP_TERMINAL_ANCHOR).search(loop)
        if terminal is None:
            err(f".claude-plugin/commands/collab.md: the dispatch-loop "
                f"skeleton no longer contains "
                f"{DISPATCH_LOOP_TERMINAL_ANCHOR!r}, so the `PlanLocked` "
                f"pre-`task_list` branch can no longer be ordered against the "
                f"exit it exists to pre-empt — re-anchor "
                f"DISPATCH_LOOP_TERMINAL_ANCHOR")
        elif branch is None:
            err(f".claude-plugin/commands/collab.md: the dispatch-loop "
                f"skeleton is missing "
                f"{PLAN_LOCKED_REACHABILITY_BRANCH_ANCHOR!r} — without that "
                f"branch the loop exits at `PlanLocked` on the v1 terminal "
                f"set and the approval gate is never reached")
        elif branch.start() > terminal.start():
            err(f".claude-plugin/commands/collab.md: the `PlanLocked` "
                f"pre-`task_list` branch "
                f"({PLAN_LOCKED_REACHABILITY_BRANCH_ANCHOR!r}) must be tested "
                f"BEFORE {DISPATCH_LOOP_TERMINAL_ANCHOR!r}. `PlanLocked` is "
                f"in the v1 terminal set, so a branch placed after the "
                f"terminal test is dead code: the loop logs "
                f"`t10_session_complete` and exits with `final_plan_hash` set "
                f"and the human never asked, which is exactly the state the "
                f"branch exists to prevent")
    matching = [l for l in text.splitlines()
                if l.startswith(PLAN_FINALIZE_ROW_PREFIX)]
    if len(matching) != PLAN_FINALIZE_ROW_COUNT:
        err(f"collab.md: expected exactly {PLAN_FINALIZE_ROW_COUNT} lines "
            f"starting with {PLAN_FINALIZE_ROW_PREFIX!r} — the approval-gate "
            f"audit selects the phase-action row by its "
            f"{PLAN_FINALIZE_ROW_MARKER!r} cell, so a restructured table "
            f"means it is reading a different one than it was written for, "
            f"found {len(matching)}")
    # The negative half of the move applies to EVERY `PlanClaudeFinalizePending`
    # row, not just the phase-action one. `PLAN_FINALIZE_ROW_MARKER` selects one
    # of the two rows this check itself asserts exist; the other — the Codex
    # dispatch tuning row — describes the same turn under `pilot == "codex"`,
    # which is the precise configuration where a gate here is unreachable. It
    # was never scanned.
    for line in matching:
        if PLAN_MODE_GATE_RE.search(line):
            err(f"collab.md: `PlanClaudeFinalizePending` rows must not take "
                f"the human planning gate — the gate moved to the "
                f"`PlanLocked` bridge, and a gate on any row for this phase "
                f"is unreachable under `pilot == \"codex\"`, where Codex owns "
                f"the finalize turn and cannot prompt a human")
    rows = [l for l in matching if PLAN_FINALIZE_ROW_MARKER in l]
    if len(rows) != 1:
        err(f"collab.md: expected exactly one `PlanClaudeFinalizePending` row "
            f"naming {PLAN_FINALIZE_ROW_MARKER!r} (the phase-action row), "
            f"found {len(rows)}")
        return
    row = rows[0]
    for phrase in PLAN_FINALIZE_ROW_SNIPPETS:
        if not flex(phrase).search(row):
            err(f"collab.md: `PlanClaudeFinalizePending` row must state that "
                f"the turn is autonomous and that the gate fires at "
                f"`PlanLocked` — missing {phrase!r}")


def check_codex_shim_unrouted_phases_contract() -> None:
    """The Codex shim's phase table must route nothing at `PlanLocked`.

    The other half of the dispatcher-owned approval gate, and the half that
    lives in a different file than the gate itself. The gate is only
    unbypassable while `PlanLocked` is dispatched by exactly one thing —
    Claude's always-on dispatcher — so the shim's *absence* of a row is load
    bearing, and an absence is what nobody notices deleting. Checked against
    the table alone rather than the whole file: the shim discusses
    `PlanLocked` in prose legitimately, and only a routing row is the bug.
    """
    if not CODEX_COMMAND.exists():
        err(".codex-plugin/commands/collab.md: missing Codex slash command, "
            "so the shim half of the dispatcher approval-gate contract "
            "(`PlanLocked` routes to nothing) is unpinned")
        return
    shim = live_text(CODEX_COMMAND)
    # Outside the table, in two shapes the table check cannot see.
    #
    # A row under any other header: the audit below proves the phase is absent
    # from the one `| Phase | Prompt |` table, and a row moved under a second,
    # differently-headed table routes exactly as well while satisfying it.
    for line_no, line in enumerate(shim.splitlines(), 1):
        if not line.lstrip().startswith("|"):
            continue
        for phase in CODEX_UNROUTED_PHASES:
            if phase in line:
                err(f".codex-plugin/commands/collab.md:{line_no}: `{phase}` "
                    f"appears in a table row outside the "
                    f"{CODEX_PHASE_TABLE_HEADER!r} table. The shim discusses "
                    f"`{phase}` in prose legitimately, but a table row is a "
                    f"route wherever it sits, and a route here lets a "
                    f"Codex-terminal `join` send `task_list` with the "
                    f"dispatcher's human gate never having fired")
    # A prose routing instruction: `select <prompt>` is how this shim states
    # every non-table route it has ("For `start`, select `collab-plan-draft.md`").
    flat = " ".join(shim.split())
    for match in CODEX_UNROUTED_SELECT_RE.finditer(flat):
        err(f".codex-plugin/commands/collab.md: {match.group(0)!r} is a "
            f"routing instruction for `{CODEX_UNROUTED_PROMPT}`, which must "
            f"never be selected on the Codex side under either pilot. The "
            f"phase→prompt table is not the only place a route can be "
            f"written, and this one-shot `codex exec` cannot open Plan Mode "
            f"or prompt a human, so the dispatcher-owned planning approval "
            f"gate cannot fire here")
    # ...and the countervailing sentence itself, which was pinned by nothing.
    # Deleting it leaves the negative pins above green — they only reject a
    # route being ADDED — while the shim stops telling a `pilot == "codex"`
    # one-shot that `PlanLocked` is not its turn.
    for phrase in CODEX_UNROUTED_NEGATIVE_SNIPPETS:
        if not flex(phrase).search(shim):
            err(f".codex-plugin/commands/collab.md: missing the sentence that "
                f"keeps `{CODEX_UNROUTED_PROMPT}` unrouted — {phrase!r}. The "
                f"prompt is installed and sits on disk with a matching name; "
                f"without this the absence of a routing row reads as an "
                f"unfinished job rather than the deliberate half of the "
                f"dispatcher-owned approval gate")
    tables = markdown_tables(shim, CODEX_PHASE_TABLE_HEADER)
    if len(tables) != 1:
        err(f".codex-plugin/commands/collab.md: expected exactly one "
            f"{CODEX_PHASE_TABLE_HEADER!r} table — the unrouted-phase audit "
            f"reads that table to prove {CODEX_UNROUTED_PHASES!r} route to "
            f"nothing, so a renamed or duplicated header leaves the "
            f"dispatcher's approval gate bypassable with nothing reporting "
            f"it, found {len(tables)}")
        return
    for phase in CODEX_UNROUTED_PHASES:
        if phase in tables[0]:
            err(f".codex-plugin/commands/collab.md: the phase→prompt table "
                f"must never carry a `{phase}` row. `/collab` on the Codex "
                f"side is a one-shot `codex exec` turn that cannot open Plan "
                f"Mode or prompt a human, so the dispatcher-owned planning "
                f"approval gate cannot fire there; `{phase}` is dispatched "
                f"only by Claude's always-running dispatcher via "
                f"`collab-turn-task-list.md`, under either pilot. Routing "
                f"`{phase}` here lets a Codex-terminal `join` at `{phase}` "
                f"under `pilot == \"codex\"` send `task_list` with no human "
                f"gate ever having fired — the exact bypass that moving the "
                f"gate to the `{phase}` bridge exists to prevent. "
                f"`.codex-plugin/prompts/collab-task-list.md` is installed "
                f"but intentionally unrouted; it is not a missing row. If "
                f"this routing genuinely must change, the gate has to move "
                f"with it and this pin has to be retired deliberately")


def check_codex_start_pilot_rejection_contract() -> None:
    """Codex's `start` must reject `--pilot`, not swallow it into `<task>`.

    The shim's `join` parses `--pilot`; its `start` does not, and the two sit
    in adjacent paragraphs. Without an explicit rejection rule the natural
    reading of `start` is "everything after the subcommand is the task", so
    `/collab start --pilot=codex fix the parser` starts a session under the
    default pilot with the flag embedded in the task text — the user's stated
    role assignment silently ignored, and no error anywhere to notice it by.
    """
    if not CODEX_COMMAND.exists():
        err(".codex-plugin/commands/collab.md: missing Codex slash command, "
            "so the `start` `--pilot` rejection contract is unpinned")
        return
    shim = live_text(CODEX_COMMAND)
    for phrase in CODEX_START_PILOT_REJECTION_SNIPPETS:
        if not flex(phrase).search(shim):
            err(f".codex-plugin/commands/collab.md: missing `start` "
                f"`--pilot` rejection contract {phrase!r}")


def check_unattended_successor_guard_contract() -> None:
    """No successor may be spawned into any v1 planning phase.

    The gate at § v3 Bridge step 0 is the only human checkpoint left in v1,
    and the automated-successor path spawns `claude -p` — a one-shot with no
    human on the other end, which is the very property the gate's own premise
    says disqualifies a process from taking it. The exclusion covers all five
    planning phases rather than the gate's own, because nothing between them
    stops: a successor spawned at `PlanParallelDrafts` drives through drafts,
    synthesis, review and the autonomous finalize turn straight into the
    gate, where it stalls forever or self-approves. The cron fallback
    re-fires every minute, so it is not a one-off either.

    Both surfaces carry it, and both are checked: the doc is where the rule
    is explained and the command file is what an agent executes, so a
    one-sided edit leaves one of them authorizing what the other forbids.
    """
    for path, rel in ((COMMAND, ".claude-plugin/commands/collab.md"),
                      (DOC, "docs/COLLAB.md")):
        text = unquoted(live_text(path))
        for phrase in UNATTENDED_SUCCESSOR_SNIPPETS:
            if not flex(phrase).search(text):
                err(f"{rel}: missing unattended-successor guard {phrase!r} — "
                    f"a successor spawned into a v1 planning phase arrives at "
                    f"the single human planning gate with no human to ask")


def check_permission_allowlist_excludes_set_pilot() -> None:
    """`collab_set_pilot` must not be on the unattended successor's allowlist.

    Scoped to the allowlist bullets alone. The identifier legitimately appears
    in three other places in these files — the paragraph directly below the
    list explaining why it is absent, the generation-lease claim list, and the
    `join` authorization contract — so a file-wide negative would be wrong at
    every one of them. The rationale paragraph is pinned positively for the
    same reason `check_codex_shim_unrouted_phases_contract` pins the shim's
    "Never select" sentence: an absence nobody explains reads as an oversight,
    and the next editor completes it.
    """
    for path, rel in ((COMMAND, ".claude-plugin/commands/collab.md"),
                      (DOC, "docs/COLLAB.md")):
        text = live_text(path)
        block = permission_allowlist_block(text)
        if block is None:
            err(f"{rel}: missing {PERMISSION_ALLOWLIST_HEADING!r} bullet list, "
                f"so the negative pin keeping `collab_set_pilot` off the "
                f"unattended successor's permissions has nothing to read")
        elif PERMISSION_ALLOWLIST_REQUIRED_ENTRY not in block:
            # A negative assertion over an empty or relocated list passes
            # vacuously, which is the one failure mode a negative pin cannot
            # report on its own.
            err(f"{rel}: the permission allowlist no longer names "
                f"{PERMISSION_ALLOWLIST_REQUIRED_ENTRY!r}, so the block this "
                f"audit reads is not the tool allowlist and its negative pin "
                f"would pass on anything")
        elif PERMISSION_ALLOWLIST_FORBIDDEN_ENTRY in block:
            err(f"{rel}: `mcp__ironmem__collab_set_pilot` must not be on the "
                f"unattended successor's permission allowlist. The successor "
                f"is spawned as a bare `join ironmem collab <sid> with token "
                f"<token>`, which carries no `--pilot`, so no successor path "
                f"reaches the call — and granting it hands an unattended "
                f"one-shot the one tool that reassigns who leads planning")
        for phrase in PERMISSION_ALLOWLIST_RATIONALE_SNIPPETS:
            if not flex(phrase).search(text):
                err(f"{rel}: the permission allowlist must say why "
                    f"`collab_set_pilot` is absent — missing {phrase!r}. "
                    f"Unexplained, the omission reads as an oversight and the "
                    f"next editor adds it back")


def permission_allowlist_block(text: str) -> str | None:
    """The bullet list under the unattended-successor permission heading.

    Just the bullets: the paragraph immediately below them names
    `collab_set_pilot` on purpose, to say it is deliberately absent, so a
    block that ran to the next heading would make the negative pin
    unsatisfiable by correct prose.
    """
    start = text.find(PERMISSION_ALLOWLIST_HEADING)
    if start < 0:
        return None
    bullets: list[str] = []
    for line in text[start:].splitlines()[1:]:
        if line.startswith("- "):
            bullets.append(line)
        elif bullets and line.startswith(" ") and line.strip():
            bullets.append(line)  # a wrapped continuation of the last bullet
        elif bullets:
            break
    return "\n".join(bullets) if bullets else None


def check_bridge_blocker_contract() -> None:
    """A bridge `blocker:` ends the bridge; it is never re-dispatched.

    The task-list worker's `blocker:` is terminal for the bridge, not a
    retryable dispatch failure. The plan is immutable at `PlanLocked` — the
    approved drawer is append-only and the plan file is pinned to
    `final_plan_hash` — so a re-dispatch re-reads the same bytes and returns
    the same blocker, while the loop finds `phase == PlanLocked` with
    `task_list` still unsent and re-enters the step-0 gate, re-prompting a
    human with a plan nobody is able to change. That is an unbounded
    re-approval loop, and it is reachable only because the gate now sits
    inside the bridge: under the old placement a bridge blocker fell out to a
    phase the loop simply exited.

    The command file's copy is checked inside the bridge section, since a rule
    about what happens on a bridge blocker states nothing anywhere else.
    """
    command_text = live_text(COMMAND)
    bridge = command_section(command_text, PLAN_LOCKED_GATE_HEADING)
    if bridge is None:
        err(f".claude-plugin/commands/collab.md: missing bridge section "
            f"{PLAN_LOCKED_GATE_HEADING!r}, which owns the bridge blocker rule")
    else:
        for phrase in BRIDGE_BLOCKER_SNIPPETS + BRIDGE_BLOCKER_COMMAND_SNIPPETS:
            if not flex(phrase).search(bridge):
                err(f".claude-plugin/commands/collab.md: v3 bridge is missing "
                    f"blocker-terminates-the-bridge contract {phrase!r} — "
                    f"without it a bridge blocker falls back through the loop "
                    f"into the step-0 gate and re-prompts the human with an "
                    f"immutable plan, forever")
    doc_text = live_text(DOC)
    for phrase in BRIDGE_BLOCKER_SNIPPETS + BRIDGE_BLOCKER_DOC_SNIPPETS:
        if not flex(phrase).search(doc_text):
            err(f"docs/COLLAB.md: missing blocker-terminates-the-bridge "
                f"contract {phrase!r}")


def check_doc_bridge_gate_ownership_contract() -> None:
    """docs/COLLAB.md must state the bridge's parse/dispatch split.

    The command file states the gate as an executable step; the doc is where
    the split is defined — the parse is worker-owned, the dispatch is gated —
    and it is the only surface that says so. A doc still describing the bridge
    as wholly worker-owned is the argument a future editor would use for
    deleting step 0 from the command file as a stray manual step.
    """
    doc_text = live_text(DOC)
    for phrase in DOC_BRIDGE_GATE_SNIPPETS:
        if not flex(phrase).search(doc_text):
            err(f"docs/COLLAB.md: missing v3-bridge approval-gate contract "
                f"{phrase!r}")


def check_single_pilot_resolution_contract() -> None:
    """Role identity is resolved once per iteration, from `collab_status`.

    The dispatch loop binds `pilot` from the same `collab_status` read that
    yields `phase` and `current_owner`, derives `copilot` as its
    counterpart, and forbids every other call site from re-deriving role
    identity — from a phase name, a template filename, or a value carried
    over from the previous iteration. Two call sites disagreeing about who
    leads is split-brain routing: a local Claude worker and a `codex exec`
    dispatched for the same turn.
    """
    text = live_text(COMMAND)
    section = command_section(text, SINGLE_PILOT_RESOLUTION_HEADING)
    if section is None:
        err(f".claude-plugin/commands/collab.md: missing "
            f"{SINGLE_PILOT_RESOLUTION_HEADING!r} section, which owns the "
            f"single-pilot-resolution rule")
        return
    for phrase in SINGLE_PILOT_RESOLUTION_SNIPPETS:
        if not flex(phrase).search(section):
            err(f".claude-plugin/commands/collab.md: dispatch loop is missing "
                f"single-pilot-resolution contract {phrase!r}")


def check_pilot_doc_contract() -> None:
    """docs/COLLAB.md must define the three roles and the `pilot` surface.

    The command files use the pilot/copilot/dispatcher vocabulary without
    defining it, and nothing but this check keeps the definitions, the
    `pilot` state row, the `collab_set_pilot` tool section, the frozen wire
    names, the Codex-terminal non-goal and the N-party boundary in sync with
    the command file that depends on them.
    """
    doc_text = live_text(DOC)
    for element, snippets in DOC_PILOT_ROLE_CONTRACTS.items():
        for phrase in snippets:
            if not flex(phrase).search(doc_text):
                err(f"docs/COLLAB.md: missing {element} — {phrase!r}")


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
        # `live_text`, not raw bytes: these pins are the ONLY content gate
        # the Codex phase prompts have, and every one of them asserts a turn
        # boundary — what the turn sends, or that it deliberately sends
        # nothing. A raw-text pin is satisfied by its own epitaph: comment out
        # the send contract, write the opposite underneath, ship green.
        if prompt.exists() and required not in live_text(prompt):
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
    # Required pins read the live body — the `$SENDER` authorization guard in
    # collab-turn-submit.md / collab-turn-task-list.md is exactly the kind of
    # rule that can be demoted into an HTML comment with the opposite written
    # underneath. FORBIDDEN pins below deliberately stay on the raw text:
    # rejecting a hardcoded sender even inside a comment is stricter, not
    # weaker.
    live = HTML_COMMENT_RE.sub("", text)
    for snippet in REQUIRED_TEMPLATE_SNIPPETS.get(name, []):
        if snippet not in live:
            err(f"{name}: missing required contract snippet {snippet!r}")
    for stale_claim in FORBIDDEN_TEMPLATE_SNIPPETS.get(name, []):
        if stale_claim in text:
            err(f"{name}: forbidden stale direct-body claim {stale_claim!r}")
    if name in SENDER_PARAMETERIZED_TEMPLATES:
        for m in HARDCODED_SENDER_RE.finditer(text):
            err(f"{name}: hardcoded sender identity {m.group(0)!r} — every "
                f"send in this template must go out as `sender=\"$SENDER\"`, "
                f"verified against `collab_status.current_owner`; a literal "
                f"claude identity breaks pilot=codex sessions")
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
        # Live body: every snippet below is a Codex turn boundary — which
        # prompts exist, the one-action-per-invocation rule, the wait call —
        # and a commented-out copy satisfies none of them for the agent.
        codex_cmd_text = live_text(CODEX_COMMAND)
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
    check_task_budget_surface_contracts()
    check_finalize_abort_contracts()
    check_review_diff_fallback_contract()
    check_reset_guards()
    check_review_range_after_recovery()
    check_batch_impl_fast_path_cleanliness()
    check_topic_template_completeness()
    check_installer_covers_templates()
    check_precondition_phase_names()
    check_pr_base_resolution_contract()
    check_pr_create_failed_doc_pointer_contract()
    check_sender_dispatch_contract()
    check_codex_pilot_routing_contract()
    check_codex_pilot_compose_handoff_contract()
    check_task_list_bridge_sender_contract()
    check_planning_dispatch_failure_contract()
    check_pilot_submit_doc_contract()
    check_review_diff_trigger_detection_contract()
    check_review_lens_mutation_classification_contract()
    check_pilot_flag_parsing_contract()
    check_end_of_options_terminator_contract()
    check_terminator_usage_surfaces()
    check_no_unbounded_flag_scan()
    check_pilot_join_authorization_contract()
    check_dispatcher_approval_gate_contract()
    check_codex_shim_unrouted_phases_contract()
    check_codex_start_pilot_rejection_contract()
    check_unattended_successor_guard_contract()
    check_permission_allowlist_excludes_set_pilot()
    check_bridge_blocker_contract()
    check_doc_bridge_gate_ownership_contract()
    check_single_pilot_resolution_contract()
    check_pilot_doc_contract()

    if errors:
        print(f"collab-turn template lint FAILED (root: {ROOT}):")
        for e in errors:
            print(f"  - {e}")
        return 1
    # The root is part of the success line on purpose. `COLLAB_LINT_ROOT`
    # redirects every path this file reads, so a green run against a fixture
    # tree, a stale worktree or a half-copied checkout is otherwise
    # indistinguishable from a green run against the repo under review — and a
    # lint whose only job is to fail on missing contracts passes most
    # convincingly when it is pointed somewhere those contracts were never
    # expected.
    print(f"collab-turn template lint OK ({len(parsed)} templates, "
          f"{len(matrix)} matrix rows, root: {ROOT})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
