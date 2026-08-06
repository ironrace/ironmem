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
        "more than 10 tasks",
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
    "**Every other phase:** as in condition 5 — the planning phases are",
]
DOC_PR_BASE_SNIPPETS = [
    "does **not** require that branch to contain `base_sha`",
    "pre-range commits in the PR body",
]
DOC_PILOT_SUBMIT_SNIPPETS = [
    "$SENDER` where that template uses",
]


def command_section(text: str, heading: str) -> str | None:
    """The body of one `## ` section, heading line included.

    The `--pilot` parsing contract has to hold in EACH of the three subcommand
    sections, and each of them states it in its own words about its own
    positional (`<task>`, `<session_id>`, `<short-topic>`). A file-wide
    substring search cannot tell "all three sections carry the rule" from
    "`start` carries it three times", which is exactly the regression this
    guards: the flags were added to `start` first, and `join`/`review` were
    left parsing the old way for several revisions.
    """
    body: list[str] = []
    inside = False
    for line in text.splitlines(keepends=True):
        if line.startswith("## "):
            if inside:
                break
            inside = line.rstrip("\n") == heading
        if inside:
            body.append(line)
    return "".join(body) if body else None


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
        "only by the agent that is currently the pilot — a copilot can never "
        "promote itself",
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
    text = COMMAND.read_text()
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
    for line_no, line in enumerate(lines, 1):
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
    codex_text = CODEX_COMMAND.read_text()
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
    command_text = COMMAND.read_text()
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
    command_text = COMMAND.read_text()
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
    command_text = COMMAND.read_text()
    for snippet in DISPATCH_FAILURE_ADMISSIBILITY_SNIPPETS:
        if snippet not in command_text:
            err(".claude-plugin/commands/collab.md: missing dispatch-failure "
                f"admissibility contract {snippet!r}")


def check_pilot_submit_doc_contract() -> None:
    """docs/COLLAB.md must document `$SENDER` as a worker placeholder."""
    doc_text = DOC.read_text()
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
    text = COMMAND.read_text()
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
        text = path.read_text()
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
    text = COMMAND.read_text()
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
    matching = [l for l in text.splitlines()
                if l.startswith(PLAN_FINALIZE_ROW_PREFIX)]
    if len(matching) != PLAN_FINALIZE_ROW_COUNT:
        err(f"collab.md: expected exactly {PLAN_FINALIZE_ROW_COUNT} lines "
            f"starting with {PLAN_FINALIZE_ROW_PREFIX!r} — the approval-gate "
            f"audit selects the phase-action row by its "
            f"{PLAN_FINALIZE_ROW_MARKER!r} cell, so a restructured table "
            f"means it is reading a different one than it was written for, "
            f"found {len(matching)}")
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
    if PLAN_MODE_GATE_RE.search(row):
        err(f"collab.md: `PlanClaudeFinalizePending` row must not take the "
            f"human planning gate — the gate moved to the `PlanLocked` "
            f"bridge, and a gate on this row is unreachable under "
            f"`pilot == \"codex\"`, where Codex owns the finalize turn and "
            f"cannot prompt a human")


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
    tables = markdown_tables(CODEX_COMMAND.read_text(),
                             CODEX_PHASE_TABLE_HEADER)
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
    text = COMMAND.read_text()
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
    doc_text = DOC.read_text()
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
    check_reset_guards()
    check_review_range_after_recovery()
    check_batch_impl_fast_path_cleanliness()
    check_topic_template_completeness()
    check_installer_covers_templates()
    check_precondition_phase_names()
    check_pr_base_resolution_contract()
    check_sender_dispatch_contract()
    check_codex_pilot_routing_contract()
    check_codex_pilot_compose_handoff_contract()
    check_task_list_bridge_sender_contract()
    check_planning_dispatch_failure_contract()
    check_pilot_submit_doc_contract()
    check_review_diff_trigger_detection_contract()
    check_pilot_flag_parsing_contract()
    check_pilot_join_authorization_contract()
    check_dispatcher_approval_gate_contract()
    check_codex_shim_unrouted_phases_contract()
    check_single_pilot_resolution_contract()
    check_pilot_doc_contract()

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
