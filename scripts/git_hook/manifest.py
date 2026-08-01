"""The pure data layer: surfaces, classification, the gate manifest.

No I/O, no env, no clock, no cwd. `collect` produces the `ChangeSet` this
module classifies; `execute` runs what `resolve_gates` selects. Everything
here is total and side-effect free, which is why the whole
collect -> resolve -> execute pipeline can be reasoned about one stage at a
time.

Kept in one module on purpose despite its length: the rationale comments here
are densely cross-referential (`is_docs_path` explains itself by pointing at
`is_gate_covered_plugin_path`, the `SURFACES` ordering comment explains why
`is_inert_config_path` may safely be a catch-all, `Gate.__post_init__`'s
domain validation explains why `_ordered_surfaces` is left unguarded).
Splitting them apart would turn same-file adjacency into cross-module lookups
for no gain in logic density -- this module is far more rationale than code.
"""
from __future__ import annotations

import dataclasses
import pathlib
from types import MappingProxyType
from typing import Callable


COLLAB_EXACT_PATHS = {
    ".claude-plugin/commands/collab.md",
    ".codex-plugin/commands/collab.md",
    ".codex-plugin/prompts/collab-plan-draft.md",
    ".codex-plugin/prompts/collab-plan-review.md",
    ".codex-plugin/prompts/collab-global-review.md",
    ".codex-plugin/prompts/collab-recovery.md",
    ".codex-plugin/prompts/collab-batch-impl.md",
    "docs/COLLAB.md",
    "scripts/check_collab_turn_templates.py",
    "scripts/install-ironmem.sh",
}

# Every file that implements the hook itself, so editing any of them selects
# the hook self-test and install-drift gates.
#
# The `scripts/git_hook/` entries are load-bearing, not bookkeeping: this
# module used to BE `scripts/run_git_hook.py`, and when it was split the
# extracted modules would otherwise have classified `inert_config`/UNKNOWN and
# stopped triggering their own test suite -- a change to the gate resolver
# skipping the gate that validates the gate resolver. Pinned by
# `test_hook_exact_paths_covers_every_git_hook_module`, which walks the
# directory rather than trusting this list to be maintained by hand.
HOOK_EXACT_PATHS = {
    ".githooks/pre-commit",
    ".githooks/pre-push",
    "scripts/install-git-hooks.sh",
    "scripts/run_git_hook.py",
    "scripts/test_run_git_hook.py",
    "scripts/git_hook/__init__.py",
    "scripts/git_hook/collect.py",
    "scripts/git_hook/execute.py",
    "scripts/git_hook/manifest.py",
    "scripts/git_hook/runtime.py",
}

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


# Top-level directory holding Cargo crates that are deliberately OUTSIDE the
# root workspace (every one of them is in that manifest's `exclude` list), so
# no `--workspace`/`--all` cargo gate compiles, lints, or formats them. Named
# here because `is_rust_path` is checked before the inert surface: without this
# yield, benchmarks Rust source would classify `rust_workspace` and select
# three slow gates that cannot observe the change.
#
# Load-bearing precondition, pinned by `test_every_benchmarks_crate_is_
# workspace_excluded`: a benchmarks crate that joins `members` IS gate-covered,
# and treating it as inert would let a non-compiling workspace push clean.
# That test fails loudly at the manifest fact rather than letting this
# classifier fail open.
_WORKSPACE_EXCLUDED_ROOT = "benchmarks"


def is_rust_path(path: str) -> bool:
    # Segment-based, never a substring: `benchmarksish/src/lib.rs` is an
    # ordinary workspace-shaped path and must still classify `rust_workspace`.
    if path.split("/", 1)[0] == _WORKSPACE_EXCLUDED_ROOT:
        return False
    name = pathlib.PurePosixPath(path).name
    return (
        path.endswith(".rs")
        or name in {"Cargo.toml", "Cargo.lock", "build.rs"}
        or path.startswith(".cargo/")
    )


def is_collab_protocol_path(path: str) -> bool:
    return (
        path in COLLAB_EXACT_PATHS
        or path.startswith(".claude-plugin/prompts/collab-turn-")
        or path.startswith("tests/collab_turn_templates/")
    )


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


def is_hook_path(path: str) -> bool:
    return path in HOOK_EXACT_PATHS


def is_workflow_path(path: str) -> bool:
    """The Workflow-harness surface: an executable `Workflow` script inside a
    harness plugin root's `workflows/` directory -- today just
    `.claude-plugin/workflows/ultrareview.js`, which edits users' working
    trees and is covered only by its own behaviour self-test.

    Unlike `is_skills_path`, there is no canonical top-level `workflows/`
    tree to also match: `.claude-plugin/workflows/` is the only one that
    exists, so this stays scoped to gate-covered plugin roots.
    `.github/workflows/*.yml` cannot match: `.github` does not end in
    `-plugin` (see `_is_gate_covered_plugin_segment`), so it never reaches
    this predicate's `.js` check regardless of the `workflows` segment name.

    Without this surface, `.claude-plugin/workflows/*.js` would still be
    correctly excluded from SURFACE_DOCS and SURFACE_INERT_CONFIG by
    `is_gate_covered_plugin_path` -- but it would then match no specific
    surface either and fall through to UNKNOWN, escalating every edit to
    every gate. That is safe but not the deliberate, provable selection the
    ultrareview workflow self-test gate needs: this predicate is what lets
    `resolve_gates` choose that gate specifically, not merely sweep it in as
    a side effect of full escalation.
    """
    segments = path.split("/")
    return (
        len(segments) >= 3
        and _is_gate_covered_plugin_segment(segments[0])
        and segments[1] == "workflows"
        and path.endswith(".js")
    )


# A harness plugin root -- `.claude-plugin/`, `.codex-plugin/`,
# `.gemini-plugin/`, `.grok-plugin/`, and any future one -- is NOT an inert
# surface, even though its contents are JSON/shell/Markdown, the exact
# formats `is_inert_config_path`/`is_docs_path` otherwise treat as unread.
# `cargo test --workspace` reads every one of those formats inside a plugin
# root:
#   - `crates/ironmem/tests/plugin_metadata.rs`'s `read_json` panics on
#     invalid JSON in `plugin.json`/`hooks.json`/`.mcp.json`, and
#     `plugin_versions_match_cargo_toml` asserts `plugin.json`'s `version`
#     equals `CARGO_PKG_VERSION` -- a version-sync defect there is exactly
#     the kind of thing this manifest exists to catch.
#   - `crates/ironmem/src/hook.rs` parses `.claude-plugin/hooks/hooks.json`
#     and asserts its `UserPromptSubmit` command string.
#   - `crates/ironmem/src/harness/packaging.rs`'s `REQUIRED_ASSETS`
#     -- a fixed literal list, identical for every harness and never
#     `<id>`-templated: `bin/ironmem-mcp.sh`, `hooks/ironmem-hook.sh`,
#     `plugin.json` -- is enforced
#     against the real repo root by `packaging_coverage_passes_for_production_registry`.
#   - `plugin_metadata.rs`'s `claude_review_agents_advertise_lean_profile`
#     parses YAML frontmatter from `.claude-plugin/agents/*.md` and fails if
#     `tools:` is missing or lists an ironmem tool.
# Every plugin-root path is therefore gate-covered and must classify UNKNOWN
# (escalate), never `docs` or `inert_config`.
def _is_gate_covered_plugin_segment(segment: str) -> bool:
    """True if `segment` has the shape of a harness plugin root: starts with
    ``.`` and ends with ``-plugin``.

    A whole-segment check, not a substring one: ``.claude-plugin-backup``
    ends with ``-backup``, not ``-plugin``, so it does not match -- the
    look-alike directory a future plugin backup/staging copy might use stays
    correctly classified by the ordinary docs/inert-config rules.
    """
    return segment.startswith(".") and segment.endswith("-plugin")


def is_gate_covered_plugin_path(path: str) -> bool:
    """True when `path`'s leading ``/``-split segment is a harness plugin
    root (see the module comment above this function for which `cargo test`
    assertions read plugin-root JSON/shell/Markdown content).

    Matched on ``path.split("/", 1)[0]``, never a substring, for the same
    byte-exact-segment reason `is_docs_path`/`is_inert_config_path` match
    ``docs``/``site``/``benchmarks`` that way.
    """
    return _is_gate_covered_plugin_segment(path.split("/", 1)[0])


def is_docs_path(path: str) -> bool:
    """Explicitly inert documentation surface: any Markdown file, or any path
    under a top-level ``docs/`` directory -- unless it is inside a
    gate-covered plugin root (see `is_gate_covered_plugin_path`), in which
    case it is NOT inert: `.claude-plugin/agents/*.md` frontmatter is parsed
    and asserted by `plugin_metadata.rs`'s lean-profile guard test.

    Matched on the leading ``/``-split segment (``path.split("/", 1)[0]``),
    never on ``"docs" in path`` -- a substring check would wrongly match a
    look-alike directory such as ``docsite/notes.txt``.
    """
    if is_gate_covered_plugin_path(path):
        return False
    return path.endswith(".md") or path.split("/", 1)[0] == "docs"


# Extensions no gate in GATES parses, executes, or otherwise inspects
# *outside a gate-covered plugin root* (see `is_gate_covered_plugin_path`):
# none of the five gates (hook self-test, install-drift check, collab
# template lint, cargo fmt/clippy/test) reads JSON/YAML/shell/CSV
# content when it lives outside `.claude-plugin/`-shaped directories -- but
# `cargo test --workspace` does read exactly these formats inside one (see
# the module comment above `is_gate_covered_plugin_path`). A
# `str.endswith()` tuple argument is a byte-exact suffix check, same as
# `is_docs_path`'s `.md` check -- no case-folding.
_INERT_CONFIG_EXTENSIONS = (
    ".json",
    ".jsonc",
    ".jsonl",
    ".yaml",
    ".yml",
    ".sh",
    ".csv",
)


def is_inert_config_path(path: str) -> bool:
    """Second explicitly-inert surface: non-code config/data files that no
    declared gate would catch a defect in -- outside a gate-covered plugin
    root.

    Checked first, before any of the three patterns below: `path` inside a
    gate-covered plugin root (see `is_gate_covered_plugin_path`) is NEVER
    inert, regardless of extension. `cargo test --workspace` reads
    plugin-root JSON (`plugin_metadata.rs`), asserts plugin-root shell
    assets exist (`packaging.rs`'s `REQUIRED_ASSETS`), and parses
    plugin-root agent Markdown frontmatter (`plugin_metadata.rs`'s
    lean-profile guard) -- a defect in any of those is gate-covered, so
    those paths must classify UNKNOWN (escalate), not `inert_config`.

    Three patterns, each justified by "no existing gate parses, executes, or
    otherwise inspects this file outside a gate-covered plugin root":

    - A file extension in ``_INERT_CONFIG_EXTENSIONS`` (JSON/YAML/shell/CSV)
      -- none of the five gates reads these formats there. HTML is deliberately
      excluded: `crates/ironmem/src/dashboard/index.html` is compiled into the
      binary with `include_str!`, so an HTML change must stay UNKNOWN and run
      the Rust gates.
    - Any path under a top-level ``site/`` directory (the static site,
      entirely outside the Rust workspace and the hook/collab scope),
      matched the same way ``is_docs_path`` matches ``docs/``: on the
      leading ``/``-split segment, never a substring, so a look-alike
      directory such as ``sitehost/`` never matches.
    - Any path under a top-level ``benchmarks/`` directory -- the whole tree,
      including its ``.rs`` source and ``Cargo.toml`` files. Every
      ``benchmarks/*`` Cargo crate is in the root workspace manifest's
      ``exclude`` list, so `cargo fmt --all`/`cargo clippy --workspace`/
      `cargo test --workspace` never compile, lint, or format any of them:
      a defect there is not gate-covered, and classifying it
      ``rust_workspace`` (as this surface previously left `is_rust_path` to
      do) selected three slow gates that could not observe the change.
      `is_rust_path` now yields for this root (see `_WORKSPACE_EXCLUDED_ROOT`)
      so the classification actually reaches here. The precondition that
      makes this safe -- the exclude list staying complete -- is not visible
      to this predicate and is pinned by
      `test_every_benchmarks_crate_is_workspace_excluded`.

      `crates/*/migrations/*.sql` is deliberately NOT covered by any pattern
      here: those files are `include_str!`'d into the Rust binary and replayed
      by `cargo test`'s migration tests, so a real gate *does* catch a defect
      there -- they must stay `UNKNOWN` (escalate), not become inert.

    `.github/workflows/*.yml` matches the extension pattern above and is
    therefore inert. That is a deliberate, verified decision, not an oversight:
    no declared gate reads `.github/` at all (no Rust test references it), so
    escalating a workflow edit would run the cargo gates -- which cannot parse
    YAML and cannot detect a broken workflow -- purely as wasted wall-clock.
    A workflow defect has no local backstop either way; CI is its own backstop
    and surfaces the failure on the very push that introduced it. Adding a
    hook-time `actionlint`/PyYAML gate was considered and rejected: neither is
    in the standard library or guaranteed installed, so it would either break
    commits on machines lacking it or have to skip when absent -- a fail-OPEN
    gate, which is worse than no gate in a manifest built on failing closed.

    Declared after every specific surface in `SURFACES` (see the ordering
    comment there): `scripts/install-git-hooks.sh` matches this predicate's
    `.sh` check too, but `SURFACE_HOOK_SELF_TEST`'s exact-path predicate is
    checked first and wins -- the same ordering protection that lets
    `docs/COLLAB.md` win to `collab_protocol` over the generic `docs`
    catch-all.
    """
    if is_gate_covered_plugin_path(path):
        return False
    if path.endswith(_INERT_CONFIG_EXTENSIONS):
        return True
    top_segment = path.split("/", 1)[0]
    if top_segment == "site":
        return True
    if top_segment == _WORKSPACE_EXCLUDED_ROOT:
        return True
    return False


# --- Frozen data model -------------------------------------------------
#
# `Gate`/`ChangeSet`/`GATES`/`SURFACES` are the pure data layer the
# collect -> resolve -> execute pipeline reads. `main()` (bottom of this
# file) is what wires collection, `resolve_gates`, and `execute_gates`
# together; nothing in this section performs I/O itself.

PHASE_PRE_COMMIT = "pre-commit"
PHASE_PRE_PUSH = "pre-push"

SURFACE_RUST_WORKSPACE = "rust_workspace"
SURFACE_COLLAB_PROTOCOL = "collab_protocol"
SURFACE_HOOK_SELF_TEST = "hook_self_test"
SURFACE_SKILLS = "skills"
SURFACE_WORKFLOWS = "workflows"
SURFACE_DOCS = "docs"
SURFACE_INERT_CONFIG = "inert_config"

# Declared phase vocabulary, used by `Gate.__post_init__`'s domain validation,
# `resolve_gates()`'s fail-loud guard, and `_cli_main`. Deliberately not
# GATES-derived: an empty or mistyped manifest must not silently widen or
# narrow which phase strings are considered valid. Declared here, above the
# `GATES` manifest, because `Gate.__post_init__` reads it while that manifest
# is being constructed at import time.
_KNOWN_PHASES = frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH})

# Not a declared surface -- the fail-closed fallback classify_path() returns
# when a path is unsafe-shaped or matches no entry in SURFACES (including
# DOCS). Deliberately absent from SURFACES: unlike DOCS, UNKNOWN is not a
# recognized surface later stages select gates for -- it is the signal that
# forces every gate for the phase to run.
UNKNOWN = "unknown"


@dataclasses.dataclass(frozen=True)
class Gate:
    """One subprocess invocation, gated by phase and changed surface."""

    name: str
    argv: tuple[str, ...]
    phases: frozenset[str]
    surfaces: frozenset[str]
    always: bool

    def __post_init__(self) -> None:
        if not isinstance(self.name, str):
            raise TypeError(f"Gate.name must be a str, got {type(self.name).__name__}")
        if not isinstance(self.argv, tuple):
            raise TypeError(f"Gate.argv must be a tuple, got {type(self.argv).__name__}")
        if not isinstance(self.phases, frozenset):
            raise TypeError(f"Gate.phases must be a frozenset, got {type(self.phases).__name__}")
        if not isinstance(self.surfaces, frozenset):
            raise TypeError(
                f"Gate.surfaces must be a frozenset, got {type(self.surfaces).__name__}"
            )
        if not isinstance(self.always, bool):
            raise TypeError(f"Gate.always must be a bool, got {type(self.always).__name__}")

        # Domain validation, not just shape. A type-correct typo is silent
        # and permanent: `phases={"pre-comit"}` constructs cleanly and the
        # gate then never runs in any phase, with no error and no skip line;
        # `surfaces={"rust_workspce"}` surfaces much later as a bare KeyError
        # out of `_SURFACE_ORDER`, far from the manifest line at fault. Both
        # are raised here so a bad manifest fails at import time, naming the
        # gate and the offending value. `SURFACES` and `_KNOWN_PHASES` are
        # both declared above the `GATES` manifest for exactly this reason.
        if not self.name:
            raise ValueError("Gate.name must be a non-empty str")
        if not self.argv:
            raise ValueError(f"Gate {self.name!r}: argv must be a non-empty tuple")
        if not self.phases:
            raise ValueError(f"Gate {self.name!r}: phases must be a non-empty frozenset")
        unknown_phases = self.phases - _KNOWN_PHASES
        if unknown_phases:
            raise ValueError(
                f"Gate {self.name!r}: unknown phase(s) {sorted(unknown_phases)}; "
                f"declared phases are {sorted(_KNOWN_PHASES)}"
            )
        if not self.surfaces and not self.always:
            # An `always=True` gate legitimately declares no surface (it runs
            # regardless of what changed and never prints a skip line). A
            # surface-selected gate with no surface would simply never be
            # selected -- the same silent no-op a misspelled phase produces.
            raise ValueError(
                f"Gate {self.name!r}: surfaces must be a non-empty frozenset "
                "unless the gate is always=True"
            )
        unknown_surfaces = self.surfaces - frozenset(SURFACES)
        if unknown_surfaces:
            raise ValueError(
                f"Gate {self.name!r}: unknown surface(s) {sorted(unknown_surfaces)}; "
                f"declared surfaces are {sorted(SURFACES)}"
            )


@dataclasses.dataclass(frozen=True)
class ChangeSet:
    """The changed paths for a phase, plus escalation state.

    `unknown=True` (with `reason` set) marks an unsafe or unrecognized path
    shape that must escalate to running every gate, never be sanitized away.
    """

    paths: tuple[str, ...]
    unknown: bool
    reason: str | None

    def __post_init__(self) -> None:
        if not isinstance(self.paths, tuple):
            raise TypeError(f"ChangeSet.paths must be a tuple, got {type(self.paths).__name__}")
        if not isinstance(self.unknown, bool):
            raise TypeError(
                f"ChangeSet.unknown must be a bool, got {type(self.unknown).__name__}"
            )
        if self.reason is not None and not isinstance(self.reason, str):
            raise TypeError(
                f"ChangeSet.reason must be a str or None, got {type(self.reason).__name__}"
            )
        # Domain validation: escalation without an explanation contradicts
        # this class's own docstring and makes a full gate run look
        # arbitrary. `execute_gates` prints `reason` only when it is truthy,
        # so an empty string is the same silent escalation as None -- both
        # are rejected at construction rather than discovered at read time.
        if self.unknown and not self.reason:
            raise ValueError("ChangeSet.unknown=True requires a non-empty reason")


# surface_id -> predicate. Predicates ported unchanged from the existing
# is_rust_path/is_collab_protocol_path/is_hook_path classifiers above, plus
# is_skills_path, is_workflow_path, the DOCS surface's is_docs_path, and the
# INERT_CONFIG surface's is_inert_config_path. Iteration order is declaration
# order: classify_path()
# below checks the specific surfaces first and the two generic inert
# catch-alls (DOCS, then INERT_CONFIG) last, so a more specific surface (e.g.
# collab protocol) wins over a generic inert catch-all when a path happens to
# satisfy both (docs/COLLAB.md is both under docs/ and in the collab-protocol
# exact set; scripts/install-git-hooks.sh is both a .sh file and in the
# hook_self_test exact set -- HOOK_SELF_TEST is declared, and therefore
# checked, before INERT_CONFIG, so it wins).
SURFACES: MappingProxyType[str, Callable[[str], bool]] = MappingProxyType(
    {
        SURFACE_RUST_WORKSPACE: is_rust_path,
        SURFACE_COLLAB_PROTOCOL: is_collab_protocol_path,
        SURFACE_HOOK_SELF_TEST: is_hook_path,
        SURFACE_SKILLS: is_skills_path,
        SURFACE_WORKFLOWS: is_workflow_path,
        SURFACE_DOCS: is_docs_path,
        SURFACE_INERT_CONFIG: is_inert_config_path,
    }
)

# Control bytes (codepoint < 0x20, plus DEL 0x7F) are rejected as unsafe path
# shapes, except the two line terminators: Git's `-z`/NUL-delimited output can
# legitimately carry an embedded LF *or* CR inside a filename, and both bytes
# must classify normally rather than be treated as an attack shape.
#
# Both, not just LF. A POSIX filename may contain any byte except NUL and `/`,
# which makes CR exactly as ordinary inside one as LF; `-z` framing removes the
# only reason either was ever a parsing hazard. Allowing LF while rejecting CR
# escalated a correctly-classifiable path to a full gate run for no stated
# reason -- an asymmetry, not a defense.
#
# The allowance is deliberately exactly this wide. Every other control byte
# still escalates: not because it could redirect anything (paths here are
# never printed, shelled out, or interpolated into a command), but because a
# path Git would not normally produce is a signal the collection layer's
# assumptions may not hold, and escalating is the fail-closed response.
_ALLOWED_CONTROL_CHARS = frozenset({"\n", "\r"})


def _unsafe_path_reason(path: object) -> str | None:
    """Return a reason string if `path` is an unsafe or malformed shape for
    classification, else None.

    Never cleans, strips, or rewrites `path` -- callers must escalate on a
    non-None reason, not sanitize and continue. Sanitizing an
    attacker-influenced path would let classification diverge from what Git
    actually staged.
    """
    if not isinstance(path, str):
        return f"non-str path: {type(path).__name__}"
    if path == "":
        return "empty path"
    if path.startswith("/"):
        return "absolute path"
    if path.startswith("-"):
        return "path starts with '-'"
    for char in path:
        if char in _ALLOWED_CONTROL_CHARS:
            continue
        codepoint = ord(char)
        if codepoint < 0x20 or codepoint == 0x7F:
            return "control byte in path"
    if ".." in path.split("/"):
        return "'..' path segment"
    return None


def classify_path(path: object) -> str:
    """Classify a single Git-reported path into a declared surface id, or
    UNKNOWN.

    Total: never raises regardless of input shape or type. Pure: never
    mutates its argument and performs no `.strip()`/unquoting/case-folding --
    matching is byte-exact on `/`-split segments, not substrings, so a
    look-alike like `docsite/`, `sitehost/`, `benchmarksish/`, or `contests/`
    never matches the surface it resembles. Unsafe shapes (absolute paths,
    `..` segments, NUL/control bytes, empty string, leading `-`, non-`str`)
    are rejected to UNKNOWN before any surface is checked, never sanitized.
    DOCS and INERT_CONFIG are declared surfaces like any other, not a
    fallback; UNKNOWN is the true fallback, returned only when the shape is
    unsafe or no declared surface (including DOCS and INERT_CONFIG) matches.
    """
    if _unsafe_path_reason(path) is not None:
        return UNKNOWN
    for surface_id, predicate in SURFACES.items():
        if predicate(path):
            return surface_id
    return UNKNOWN


# Declaration order IS execution order. Never sorted at runtime. Ported
# unchanged from the pre-Task-6 run_pre_commit()/run_pre_push() conditional
# assembly this manifest replaced (same argv, same phase membership).
GATES: tuple[Gate, ...] = (
    Gate(
        name="hook_self_test",
        argv=("python3", "scripts/test_run_git_hook.py"),
        phases=frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_HOOK_SELF_TEST}),
        always=False,
    ),
    Gate(
        name="hook_install_check",
        argv=("bash", "scripts/install-git-hooks.sh", "--check"),
        phases=frozenset({PHASE_PRE_COMMIT}),
        surfaces=frozenset({SURFACE_HOOK_SELF_TEST}),
        always=False,
    ),
    Gate(
        name="collab_template_lint",
        argv=("python3", "scripts/check_collab_turn_templates.py"),
        phases=frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_COLLAB_PROTOCOL}),
        always=False,
    ),
    Gate(
        name="skills_sync_check",
        argv=("python3", "scripts/check_skills_sync.py"),
        phases=frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_SKILLS}),
        always=False,
    ),
    Gate(
        name="rust_fmt_check",
        argv=("cargo", "fmt", "--all", "--", "--check"),
        phases=frozenset({PHASE_PRE_COMMIT}),
        surfaces=frozenset({SURFACE_RUST_WORKSPACE}),
        always=False,
    ),
    Gate(
        name="rust_clippy",
        argv=(
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ),
        phases=frozenset({PHASE_PRE_COMMIT}),
        surfaces=frozenset({SURFACE_RUST_WORKSPACE}),
        always=False,
    ),
    Gate(
        name="rust_test",
        argv=("cargo", "test", "--workspace"),
        phases=frozenset({PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_RUST_WORKSPACE}),
        always=False,
    ),
    # Task 6 (ultrareview v2): the workflow harness in
    # scripts/test_ultrareview_workflow.mjs is the only automated coverage
    # `.claude-plugin/workflows/ultrareview.js` has -- that script edits
    # users' working trees, so an unwired check here would be the exact
    # silent miss the module docstring's HOOK_EXACT_PATHS comment warns
    # about, one surface over. Not ported from the pre-manifest hook
    # scripts: this gate is new, added alongside the harness itself.
    Gate(
        name="ultrareview_workflow_self_test",
        argv=("node", "scripts/test_ultrareview_workflow.mjs"),
        phases=frozenset({PHASE_PRE_COMMIT, PHASE_PRE_PUSH}),
        surfaces=frozenset({SURFACE_WORKFLOWS}),
        always=False,
    ),
)


def resolve_gates(phase: str, changes: ChangeSet) -> tuple[Gate, ...]:
    """Select the gates in ``GATES`` that must run for ``phase`` given ``changes``.

    Pure and total: no I/O, no env, no clock, no cwd -- every input is the two
    arguments, every output is a new tuple built fresh from ``GATES`` in
    declaration order. Never mutates ``changes`` or ``GATES``. Output order is
    manifest order, invariant to input path order and duplicates.

    Byte-preserving: every path in ``changes.paths`` is classified exactly as
    received. Deduping via ``dict.fromkeys`` compares paths byte-for-byte
    (never after ``.strip()``/unquoting/case-folding), and ``classify_path``
    itself never rewrites a path before matching it against ``SURFACES`` --
    an unsafe or attacker-influenced path is escalated to ``UNKNOWN``, never
    sanitized and reclassified. This is deliberate: rewriting a path before
    classifying it would let this function's decision disagree with what Git
    actually staged.

    A gate is selected when ``phase in gate.phases`` and at least one of:
    - ``gate.always`` is True, or
    - ``changes.unknown`` is True (the collection layer could not determine
      the real change set and fails closed), or
    - classifying ``changes.paths`` (deduped by first-seen via
      ``dict.fromkeys``, never by sorting) yields ``UNKNOWN`` for any path --
      an unsafe or unrecognized path shape fails closed exactly like
      ``changes.unknown``, forcing every phase-matching gate to run, or
    - the set of surfaces those paths classify to intersects ``gate.surfaces``.

    ``SURFACE_DOCS`` and ``SURFACE_INERT_CONFIG`` are explicitly inert:
    classifying to either never by itself satisfies a gate's surface
    intersection, so an all-docs/all-inert-config, all-safe-shape change
    selects only ``always`` gates (none exist in today's manifest).

    Raises ``ValueError`` for a phase outside the declared phase vocabulary --
    a typo must not silently disable every gate.
    """
    if phase not in _KNOWN_PHASES:
        raise ValueError(f"unknown phase: {phase!r}")

    deduped_paths = tuple(dict.fromkeys(changes.paths))
    classified_surfaces = frozenset(classify_path(path) for path in deduped_paths)
    escalate = changes.unknown or UNKNOWN in classified_surfaces

    return tuple(
        gate
        for gate in GATES
        if phase in gate.phases
        and (gate.always or escalate or classified_surfaces & gate.surfaces)
    )




# Declaration position of each surface id in SURFACES, used only to render a
# gate's `surfaces` frozenset in a deterministic order for the skip line
# below. Frozenset iteration order depends on CPython's per-process string
# hash randomization, so printing straight from the frozenset would make
# the skip line's surfaces field vary run to run for any gate declaring more
# than one surface, even though nothing else changed.
_SURFACE_ORDER: MappingProxyType[str, int] = MappingProxyType(
    {surface_id: index for index, surface_id in enumerate(SURFACES)}
)


def _ordered_surfaces(surface_ids: frozenset[str]) -> tuple[str, ...]:
    # Every `surface_id` here always comes from a `Gate.surfaces` frozenset,
    # every surface a Gate declares is a key in `SURFACES` -- enforced at
    # construction by `Gate.__post_init__`'s domain validation, so a gate
    # declaring an unknown surface now raises ValueError at import time
    # naming the gate and the value, rather than reaching this lookup as a
    # bare KeyError. Left unguarded deliberately for the same reason: a
    # manifest bug should fail loud, not be swallowed into a
    # silently-unordered fallback.
    return tuple(sorted(surface_ids, key=lambda surface_id: _SURFACE_ORDER[surface_id]))
