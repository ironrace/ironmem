#!/usr/bin/env python3
"""Tests for scripts/run_git_hook.py.

Invoked directly by the tracked Git hooks as
``python3 scripts/test_run_git_hook.py`` (see .githooks/pre-commit and
.githooks/pre-push), and also runnable normally via ``pytest`` or
``pytest scripts/test_run_git_hook.py``.
"""
from __future__ import annotations

import dataclasses
import os
import pathlib
import subprocess
import sys
from types import MappingProxyType

try:
    import pytest
except ImportError:  # pragma: no cover - exercised via subprocess, see below
    # Fail here, at the import, not later in `_run_as_script()`.
    #
    # This module used to bind `pytest = None` and defer the friendly error to
    # `_run_as_script()`. That path was unreachable: the first
    # `@pytest.mark.parametrize` at module scope is evaluated during import, so
    # a missing pytest produced `AttributeError: 'NoneType' object has no
    # attribute 'mark'` and a traceback instead. CI ran this file for weeks in
    # exactly that state -- the hook self-test gate reported failure for a
    # missing test dependency rather than ever running, which is how a gate
    # stops gating without anyone noticing.
    sys.stderr.write(
        "ERROR: pytest is required to run scripts/test_run_git_hook.py but is "
        "not installed.\nInstall it with: pip install pytest\n"
    )
    raise SystemExit(1) from None

ROOT = pathlib.Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"

# `scripts/` on sys.path, then ordinary imports -- not the previous
# spec_from_file_location load of a single file. The hook is a package now, and
# loading `run_git_hook.py` by path would leave its `from git_hook import ...`
# unresolvable. `python3 scripts/run_git_hook.py` gets the same sys.path[0] for
# free, so the hook and its tests resolve the package identically.
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

import run_git_hook as cli  # noqa: E402
from git_hook import collect, execute, manifest, runtime  # noqa: E402

# Tests address each symbol through the module that OWNS it -- `manifest.GATES`,
# `collect._run_git`, `runtime._scrub_git_env` -- rather than through a single
# re-exporting handle. That is deliberate: `execute` reads `manifest.GATES` at
# call time, so `monkeypatch.setattr(manifest, "GATES", ...)` reaches the
# binding under test. Patching a re-export would silently leave the real
# manifest in play and the test would assert against the wrong gate set.


# --- Task 1: Gate is frozen ---


def test_gate_is_frozen():
    gate = manifest.Gate(
        name="example",
        argv=("python3", "scripts/example.py"),
        phases=frozenset({"pre-commit"}),
        surfaces=frozenset({manifest.SURFACE_HOOK_SELF_TEST}),
        always=False,
    )
    with pytest.raises(dataclasses.FrozenInstanceError):
        gate.name = "mutated"  # type: ignore[misc]


def test_gate_rejects_non_str_name():
    with pytest.raises(TypeError):
        manifest.Gate(
            name=123,  # type: ignore[arg-type]
            argv=("python3", "scripts/example.py"),
            phases=frozenset({"pre-commit"}),
            surfaces=frozenset({manifest.SURFACE_HOOK_SELF_TEST}),
            always=False,
        )


def test_gate_rejects_non_tuple_argv():
    with pytest.raises(TypeError):
        manifest.Gate(
            name="example",
            argv=["python3", "scripts/example.py"],  # type: ignore[arg-type]
            phases=frozenset({"pre-commit"}),
            surfaces=frozenset({manifest.SURFACE_HOOK_SELF_TEST}),
            always=False,
        )


def test_gate_rejects_non_frozenset_phases():
    with pytest.raises(TypeError):
        manifest.Gate(
            name="example",
            argv=("python3", "scripts/example.py"),
            phases={"pre-commit"},  # type: ignore[arg-type]
            surfaces=frozenset({manifest.SURFACE_HOOK_SELF_TEST}),
            always=False,
        )


def test_gate_rejects_non_frozenset_surfaces():
    with pytest.raises(TypeError):
        manifest.Gate(
            name="example",
            argv=("python3", "scripts/example.py"),
            phases=frozenset({"pre-commit"}),
            surfaces={manifest.SURFACE_HOOK_SELF_TEST},  # type: ignore[arg-type]
            always=False,
        )


def test_gate_rejects_non_bool_always():
    with pytest.raises(TypeError):
        manifest.Gate(
            name="example",
            argv=("python3", "scripts/example.py"),
            phases=frozenset({"pre-commit"}),
            surfaces=frozenset({manifest.SURFACE_HOOK_SELF_TEST}),
            always="false",  # type: ignore[arg-type]
        )


# --- Gate domain validation -- a manifest typo must fail loudly ------------
#
# Shape-only type checks let a typo construct a perfectly well-formed Gate
# that silently never runs (a misspelled phase) or blows up far from the
# mistake with a bare KeyError (a misspelled surface). Domain validation moves
# both failures to import time, where the offending value is still visible.


def _gate(**overrides):
    kwargs = {
        "name": "example",
        "argv": ("python3", "scripts/example.py"),
        "phases": frozenset({manifest.PHASE_PRE_COMMIT}),
        "surfaces": frozenset({manifest.SURFACE_HOOK_SELF_TEST}),
        "always": False,
    }
    kwargs.update(overrides)
    return manifest.Gate(**kwargs)


def test_gate_rejects_misspelled_phase():
    # Without this guard `pre-comit` constructs cleanly and the gate never
    # runs in any phase, with no error and no skip line.
    with pytest.raises(ValueError) as excinfo:
        _gate(phases=frozenset({"pre-comit"}))
    assert "pre-comit" in str(excinfo.value)
    assert "example" in str(excinfo.value)


def test_gate_rejects_empty_phases():
    with pytest.raises(ValueError) as excinfo:
        _gate(phases=frozenset())
    assert "example" in str(excinfo.value)


def test_gate_rejects_misspelled_surface():
    # Without this guard `rust_workspce` surfaces later as a bare KeyError
    # from _SURFACE_ORDER, far from the manifest line that caused it.
    with pytest.raises(ValueError) as excinfo:
        _gate(surfaces=frozenset({"rust_workspce"}))
    assert "rust_workspce" in str(excinfo.value)
    assert "example" in str(excinfo.value)


def test_gate_rejects_empty_surfaces_for_a_surface_selected_gate():
    with pytest.raises(ValueError) as excinfo:
        _gate(surfaces=frozenset(), always=False)
    assert "example" in str(excinfo.value)


def test_gate_allows_empty_surfaces_for_an_always_gate():
    # An always=True gate runs regardless of what changed and never prints a
    # skip line, so declaring no surface is meaningful for it -- unlike a
    # surface-selected gate, which would simply never be selected.
    gate = _gate(surfaces=frozenset(), always=True)
    assert gate.surfaces == frozenset()


def test_gate_rejects_empty_name():
    with pytest.raises(ValueError):
        _gate(name="")


def test_gate_rejects_empty_argv():
    # An empty argv reaches subprocess.run and raises outside the OSError-only
    # catch in execute_gates.
    with pytest.raises(ValueError) as excinfo:
        _gate(argv=())
    assert "example" in str(excinfo.value)


def test_gate_accepts_every_declared_phase_and_surface():
    gate = _gate(
        phases=frozenset({manifest.PHASE_PRE_COMMIT, manifest.PHASE_PRE_PUSH}),
        surfaces=frozenset(manifest.SURFACES),
    )
    assert gate.surfaces == frozenset(manifest.SURFACES)


# --- Task 1: ChangeSet is frozen ---


def test_changeset_is_frozen():
    changeset = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    with pytest.raises(dataclasses.FrozenInstanceError):
        changeset.unknown = True  # type: ignore[misc]


def test_changeset_rejects_non_tuple_paths():
    with pytest.raises(TypeError):
        manifest.ChangeSet(paths=["a.py"], unknown=False, reason=None)  # type: ignore[arg-type]


def test_changeset_rejects_non_bool_unknown():
    with pytest.raises(TypeError):
        manifest.ChangeSet(paths=(), unknown="false", reason=None)  # type: ignore[arg-type]


def test_changeset_rejects_non_str_reason():
    with pytest.raises(TypeError):
        manifest.ChangeSet(paths=(), unknown=True, reason=404)  # type: ignore[arg-type]


def test_changeset_accepts_none_reason():
    changeset = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    assert changeset.reason is None


def test_changeset_rejects_escalation_without_a_reason():
    # The class docstring says `unknown=True` comes "with `reason` set".
    # Allowing reason=None let an escalation run every gate while printing no
    # explanation at all, so a surprising full run looked arbitrary.
    with pytest.raises(ValueError):
        manifest.ChangeSet(paths=(), unknown=True, reason=None)


def test_changeset_rejects_escalation_with_an_empty_reason():
    # An empty string is the same silent escalation as None: execute_gates'
    # `if changes.unknown and changes.reason:` guard is falsy for both.
    with pytest.raises(ValueError):
        manifest.ChangeSet(paths=(), unknown=True, reason="")


def test_changeset_default_construction():
    changeset = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    assert changeset.paths == ()
    assert changeset.unknown is False
    assert changeset.reason is None


def test_changeset_default_is_distinct_from_escalated():
    default = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    escalated = manifest.ChangeSet(
        paths=("weird\x00path",), unknown=True, reason="null byte in path"
    )
    assert default != escalated
    assert escalated.unknown is True
    assert escalated.reason == "null byte in path"


# --- Task 1: manifest is a tuple, declaration order is execution order ---


def test_manifest_is_a_tuple():
    assert isinstance(manifest.GATES, tuple)
    with pytest.raises(AttributeError):
        manifest.GATES.append(manifest.GATES[0])  # type: ignore[attr-defined]


def test_manifest_declaration_order_is_preserved():
    # This is the literal authored order. If anything ever sorts GATES at
    # runtime this test must fail, because the authored order below is not
    # alphabetical (see test_manifest_declaration_order_is_not_alphabetical).
    names = [gate.name for gate in manifest.GATES]
    assert names == [
        "hook_self_test",
        "hook_install_check",
        "collab_template_lint",
        "rust_fmt_check",
        "rust_clippy",
        "rust_test",
    ]


def test_manifest_declaration_order_is_not_alphabetical():
    # Proves the preceding order-equality test is a meaningful check for "no
    # runtime sort", not an accident of already-sorted input.
    names = [gate.name for gate in manifest.GATES]
    assert names != sorted(names)


# --- Task 1: manifest gates match today's run_pre_commit/run_pre_push ---


def test_manifest_matches_pre_commit_argv_and_order():
    expected = [
        ("python3", "scripts/test_run_git_hook.py"),
        ("bash", "scripts/install-git-hooks.sh", "--check"),
        ("python3", "scripts/check_collab_turn_templates.py"),
        ("cargo", "fmt", "--all", "--", "--check"),
        (
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ),
    ]
    actual = [gate.argv for gate in manifest.GATES if "pre-commit" in gate.phases]
    assert actual == expected


def test_manifest_matches_pre_push_argv_and_order():
    expected = [
        ("python3", "scripts/test_run_git_hook.py"),
        ("python3", "scripts/check_collab_turn_templates.py"),
        ("cargo", "test", "--workspace"),
    ]
    actual = [gate.argv for gate in manifest.GATES if "pre-push" in gate.phases]
    assert actual == expected


def test_manifest_gate_argv_entries_are_string_literals():
    for gate in manifest.GATES:
        for entry in gate.argv:
            assert isinstance(entry, str)


def test_manifest_no_gate_marked_always_yet():
    # None of today's ported gates run unconditionally; `always` exists for
    # future gates but nothing in the current set uses it.
    assert all(gate.always is False for gate in manifest.GATES)


# --- Task 1: surface map ---


def test_surfaces_contains_expected_ids():
    assert set(manifest.SURFACES) == {
        manifest.SURFACE_RUST_WORKSPACE,
        manifest.SURFACE_COLLAB_PROTOCOL,
        manifest.SURFACE_HOOK_SELF_TEST,
        manifest.SURFACE_DOCS,
        manifest.SURFACE_INERT_CONFIG,
    }


def test_surfaces_ported_unchanged_from_existing_predicates():
    assert manifest.SURFACES[manifest.SURFACE_RUST_WORKSPACE] is manifest.is_rust_path
    assert manifest.SURFACES[manifest.SURFACE_COLLAB_PROTOCOL] is manifest.is_collab_protocol_path
    assert manifest.SURFACES[manifest.SURFACE_HOOK_SELF_TEST] is manifest.is_hook_path


def test_surfaces_docs_entry_is_is_docs_path():
    assert manifest.SURFACES[manifest.SURFACE_DOCS] is manifest.is_docs_path


def test_surfaces_inert_config_entry_is_is_inert_config_path():
    assert manifest.SURFACES[manifest.SURFACE_INERT_CONFIG] is manifest.is_inert_config_path


# --- Task 9: SURFACE_INERT_CONFIG declared after the specific surfaces ---
#
# Ordering is the load-bearing property here, not mere presence: a `.sh` gate
# script like scripts/install-git-hooks.sh matches is_inert_config_path's
# extension check too, so if SURFACE_INERT_CONFIG were ever declared before
# SURFACE_HOOK_SELF_TEST, classify_path() would return the wrong surface for
# every hook script -- the dict-iteration-order win, not a hardcoded
# precedence rule.


def test_surface_inert_config_declared_after_every_specific_surface():
    order = list(manifest.SURFACES)
    inert_index = order.index(manifest.SURFACE_INERT_CONFIG)
    for specific in (
        manifest.SURFACE_RUST_WORKSPACE,
        manifest.SURFACE_COLLAB_PROTOCOL,
        manifest.SURFACE_HOOK_SELF_TEST,
    ):
        assert order.index(specific) < inert_index


def test_surfaces_is_a_frozen_mapping():
    assert isinstance(manifest.SURFACES, MappingProxyType)
    with pytest.raises(TypeError):
        manifest.SURFACES["new_surface"] = lambda path: False  # type: ignore[index]


def test_every_gate_surface_id_is_registered():
    for gate in manifest.GATES:
        for surface_id in gate.surfaces:
            assert surface_id in manifest.SURFACES


# --- Task 2: is_docs_path -----------------------------------------------


def test_is_docs_path_matches_markdown_suffix():
    assert manifest.is_docs_path("README.md") is True
    assert manifest.is_docs_path("AGENTS.md") is True


def test_is_docs_path_matches_top_level_docs_directory():
    assert manifest.is_docs_path("docs/CODEX.md") is True
    assert manifest.is_docs_path("docs/superpowers/plans/notes.txt") is True


def test_is_docs_path_rejects_look_alike_directory():
    # "docsite/" is not "docs/" -- a substring check ("docs" in path) would
    # wrongly match this; a segment/prefix check must not.
    assert manifest.is_docs_path("docsite/architecture.txt") is False


def test_is_docs_path_rejects_non_markdown_non_docs_path():
    assert manifest.is_docs_path("crates/ironmem/src/hook.rs") is False


# --- Task 2: UNKNOWN is a distinct fallback, not a declared surface -----


def test_unknown_is_not_a_declared_surface():
    assert manifest.UNKNOWN not in manifest.SURFACES


# --- Task 2: classify_path -- known surfaces ----------------------------


def test_classify_path_rust_source():
    assert manifest.classify_path("crates/ironmem/src/hook.rs") == manifest.SURFACE_RUST_WORKSPACE


def test_classify_path_collab_exact_path():
    assert manifest.classify_path("scripts/check_collab_turn_templates.py") == (
        manifest.SURFACE_COLLAB_PROTOCOL
    )


def test_classify_path_install_ironmem_stays_collab_protocol():
    path = "scripts/install-ironmem.sh"
    assert manifest.classify_path(path) == manifest.SURFACE_COLLAB_PROTOCOL
    changes = manifest.ChangeSet(paths=(path,), unknown=False, reason=None)
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert "collab_template_lint" in names


def test_classify_path_codex_recovery_prompt_stays_collab_protocol():
    path = ".codex-plugin/prompts/collab-recovery.md"
    assert manifest.classify_path(path) == manifest.SURFACE_COLLAB_PROTOCOL
    changes = manifest.ChangeSet(paths=(path,), unknown=False, reason=None)
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert "collab_template_lint" in names


def test_classify_path_collab_turn_prompt_prefix_selects_collab_lint():
    # Covers the `.claude-plugin/prompts/collab-turn-` startswith branch in
    # is_collab_protocol_path -- deleting that clause must break both the
    # classification and the gate selection asserted here, not just the
    # classification.
    path = ".claude-plugin/prompts/collab-turn-plan.md"
    assert manifest.classify_path(path) == manifest.SURFACE_COLLAB_PROTOCOL
    changes = manifest.ChangeSet(paths=(path,), unknown=False, reason=None)
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert "collab_template_lint" in names


def test_classify_path_collab_turn_templates_dir_prefix_selects_collab_lint():
    # Covers the `tests/collab_turn_templates/` startswith branch in
    # is_collab_protocol_path. The existing near-miss test
    # (test_classify_path_near_miss_contests_is_not_collab_protocol) only
    # proves a *look-alike* path is rejected; it passes even if this branch
    # is deleted entirely. This test is the missing positive case.
    path = "tests/collab_turn_templates/example.txt"
    assert manifest.classify_path(path) == manifest.SURFACE_COLLAB_PROTOCOL
    changes = manifest.ChangeSet(paths=(path,), unknown=False, reason=None)
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert "collab_template_lint" in names


def test_classify_path_hook_self_test_run_git_hook():
    assert manifest.classify_path("scripts/run_git_hook.py") == manifest.SURFACE_HOOK_SELF_TEST


def test_classify_path_hook_self_test_test_run_git_hook():
    assert manifest.classify_path("scripts/test_run_git_hook.py") == manifest.SURFACE_HOOK_SELF_TEST


def test_classify_path_docs_markdown_file():
    assert manifest.classify_path("README.md") == manifest.SURFACE_DOCS


def test_classify_path_docs_directory():
    assert manifest.classify_path("docs/superpowers/plans/notes.txt") == manifest.SURFACE_DOCS


def test_classify_path_known_surface_beats_generic_docs():
    # docs/COLLAB.md is both under docs/ and in the collab-protocol exact
    # set. The more specific declared surface wins over the generic inert
    # docs catch-all -- DOCS is checked last, never first.
    assert manifest.classify_path("docs/COLLAB.md") == manifest.SURFACE_COLLAB_PROTOCOL


# --- Task 9: is_inert_config_path -- second explicitly-inert surface -----


def test_is_inert_config_path_matches_json_extension():
    assert manifest.is_inert_config_path("crates/ironmem/schema/example.json") is True


def test_is_inert_config_path_matches_yaml_and_yml_extensions():
    assert manifest.is_inert_config_path("ops/deploy.yaml") is True
    assert manifest.is_inert_config_path(".github/workflows/ci.yml") is True


def test_is_inert_config_path_matches_shell_script_extension():
    assert manifest.is_inert_config_path("scripts/check_versions.sh") is True


def test_is_inert_config_path_matches_csv_and_jsonl_and_jsonc():
    assert manifest.is_inert_config_path("benchmarks/provbench/spotcheck/sample-eaf82d2.csv") is True
    assert manifest.is_inert_config_path("benchmarks/abeval/corpus/tasks.jsonl") is True
    assert manifest.is_inert_config_path("wrangler.jsonc") is True


def test_is_inert_config_path_rejects_dashboard_html_compiled_into_binary():
    assert manifest.is_inert_config_path("crates/ironmem/src/dashboard/index.html") is False


def test_is_inert_config_path_matches_site_directory_regardless_of_extension():
    assert manifest.is_inert_config_path("site/index.html") is True
    assert manifest.is_inert_config_path("site/script.js") is True
    assert manifest.is_inert_config_path("site/_headers") is True  # no extension at all


def test_is_inert_config_path_matches_whole_benchmarks_tree():
    # CONTRACT CHANGE (was: .py under benchmarks/ only, with .rs there
    # deliberately classifying rust_workspace). Every benchmarks/* Cargo crate
    # is in the root workspace manifest's `exclude` list, so `cargo fmt --all`,
    # `cargo clippy --workspace`, and `cargo test --workspace` never compile,
    # lint, or format any of them -- a defect in benchmarks Rust source is not
    # gate-covered, and selecting the Rust gates for it ran three slow gates
    # that cannot observe the change. The whole tree is inert.
    #
    # This is only safe while that exclude list stays complete, which is not a
    # fact this predicate can see. test_every_benchmarks_crate_is_workspace_
    # excluded below is the load-bearing guard: adding a benchmarks crate to
    # `members` fails there loudly instead of failing open here silently.
    assert manifest.is_inert_config_path("benchmarks/abeval/baseline_driver.py") is True
    assert (
        manifest.is_inert_config_path("benchmarks/provbench/spotcheck/tools/autofilter.py") is True
    )
    assert manifest.is_inert_config_path("benchmarks/provbench/labeler/src/lib.rs") is True
    assert manifest.is_inert_config_path("benchmarks/abeval/Cargo.toml") is True


def test_is_rust_path_excludes_workspace_excluded_benchmarks_tree():
    # is_rust_path is checked before the inert surface, so it -- not just the
    # inert predicate -- has to yield for benchmarks/ to classify inert at all.
    assert manifest.is_rust_path("benchmarks/provbench/labeler/src/lib.rs") is False
    assert manifest.is_rust_path("benchmarks/abeval/Cargo.toml") is False
    # ...without disturbing real workspace crates or the look-alike directory.
    assert manifest.is_rust_path("crates/ironmem/src/lib.rs") is True
    assert manifest.is_rust_path("benchmarksish/src/lib.rs") is True


def test_classify_path_benchmarks_rust_source_is_inert():
    assert (
        manifest.classify_path("benchmarks/provbench/labeler/src/lib.rs")
        == manifest.SURFACE_INERT_CONFIG
    )


def test_every_benchmarks_crate_is_workspace_excluded():
    # The guard that makes the whole-benchmarks-tree inert surface safe.
    # `benchmarks/` is inert ONLY because every Cargo crate under it is
    # workspace-excluded. If a future crate there joins `members` (or a new
    # crate is added without an `exclude` entry), cargo gates WOULD cover it,
    # and treating it as inert would let a non-compiling workspace push
    # clean -- the exact fail-open this manifest exists to prevent. Fail here,
    # at the manifest fact, rather than silently in the classifier.
    #
    # Enumerated from `git ls-files` -- TRACKED files only -- never a
    # filesystem walk. The first version of this guard used
    # `(ROOT / "benchmarks").rglob("Cargo.toml")` and passed in CI and in a
    # fresh worktree while failing in a working clone, because `benchmarks/`
    # also accumulates untracked content that is nothing to do with this
    # repo's crates: abeval campaign workspaces (which contain entire cloned
    # copies of ironmem, each with its own crates/ and benchmarks/), and
    # provbench's `work/ripgrep` + `work/serde` upstream checkouts. Those are
    # not this workspace's crates and `cargo --workspace` never sees them, but
    # the walk counted every one and the guard failed with ~200 bogus paths --
    # blocking commits on any hook-file edit.
    #
    # The manifest's `exclude` list is a statement about tracked crates, so the
    # guard has to be too. `git ls-files` is also what makes this correct under
    # a `git clean`-less workflow: what is committed is what cargo builds.
    import tomllib

    root_manifest = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    workspace = root_manifest["workspace"]
    excluded = set(workspace.get("exclude", []))

    tracked = subprocess.run(
        ["git", "ls-files", "-z", "--", "benchmarks"],
        capture_output=True,
        text=True,
        cwd=ROOT,
        check=True,
    ).stdout
    crate_dirs = {
        pathlib.PurePosixPath(entry).parent.as_posix()
        for entry in tracked.split("\0")
        if entry.endswith("/Cargo.toml")
    }
    assert crate_dirs, "expected at least one tracked benchmarks Cargo crate to guard"

    unexcluded = sorted(crate_dirs - excluded)
    assert not unexcluded, (
        f"benchmarks crate(s) {unexcluded} are not in the workspace `exclude` list, "
        "so cargo gates DO cover them -- is_inert_config_path must stop treating "
        "benchmarks/ as inert, or these crates must be excluded"
    )

    members_under_benchmarks = sorted(
        member for member in workspace.get("members", []) if member.startswith("benchmarks/")
    )
    assert not members_under_benchmarks, (
        f"workspace members {members_under_benchmarks} live under benchmarks/, which "
        "the hook manifest classifies as inert -- these would skip the Rust gates"
    )


def test_is_inert_config_path_rejects_sql_migration():
    # crates/ironmem/migrations/*.sql is include_str!'d into the Rust binary
    # and exercised by cargo test's migration-replay tests -- a real gate
    # catches a defect here, so it must stay UNKNOWN (escalate), never
    # become inert.
    assert manifest.is_inert_config_path("crates/ironmem/migrations/001_init.sql") is False


def test_is_inert_config_path_rejects_look_alike_site_directory():
    # "sitehost/" is not "site/" -- segment-based matching, not substring.
    assert manifest.is_inert_config_path("sitehost/notes.txt") is False


def test_is_inert_config_path_rejects_look_alike_benchmarks_directory():
    assert manifest.is_inert_config_path("benchmarksish/tool.py") is False


def test_is_inert_config_path_rejects_non_matching_extension_outside_declared_dirs():
    assert manifest.is_inert_config_path("crates/ironmem/src/lib.rs") is False
    assert manifest.is_inert_config_path("notes.txt") is False


# --- Task 9 fix: gate-covered plugin roots are NOT inert -----------------
#
# Part A's original premise ("no existing gate would catch a defect in this
# file") is false for a harness plugin root: cargo test --workspace reads
# plugin.json/hooks.json/.mcp.json (plugin_metadata.rs's read_json and
# plugin_versions_match_cargo_toml), asserts hooks.json's UserPromptSubmit
# command (hook.rs), enforces the plugin's required shell assets exist
# (packaging.rs's REQUIRED_ASSETS via packaging_coverage_passes_for_
# production_registry), and parses review-agent Markdown frontmatter
# (plugin_metadata.rs's claude_review_agents_advertise_lean_profile). A path
# whose leading segment matches the `.<name>-plugin` shape must therefore
# never classify docs or inert_config -- it must escalate to UNKNOWN.


def test_is_gate_covered_plugin_path_matches_known_plugin_roots():
    assert manifest.is_gate_covered_plugin_path(".claude-plugin/plugin.json") is True
    assert manifest.is_gate_covered_plugin_path(".codex-plugin/hooks.json") is True
    assert manifest.is_gate_covered_plugin_path(".gemini-plugin/plugin.json") is True
    assert manifest.is_gate_covered_plugin_path(".grok-plugin/plugin.json") is True


def test_is_gate_covered_plugin_path_matches_future_plugin_shape():
    # Not an allowlist of today's four harnesses -- any future
    # `.<name>-plugin/` root matches the same shape check.
    assert manifest.is_gate_covered_plugin_path(".newharness-plugin/plugin.json") is True


def test_is_gate_covered_plugin_path_rejects_look_alike_backup_directory():
    # ".claude-plugin-backup" ends with "-backup", not "-plugin" -- a
    # substring check ("plugin" in segment) would wrongly match this; the
    # whole-segment endswith("-plugin") check must not.
    assert manifest.is_gate_covered_plugin_path(".claude-plugin-backup/x.json") is False


def test_is_gate_covered_plugin_path_rejects_non_dotted_segment():
    assert manifest.is_gate_covered_plugin_path("claude-plugin/plugin.json") is False


def test_is_gate_covered_plugin_path_rejects_ordinary_path():
    assert manifest.is_gate_covered_plugin_path("crates/ironmem/src/lib.rs") is False


def test_is_inert_config_path_rejects_plugin_json():
    assert manifest.is_inert_config_path(".claude-plugin/plugin.json") is False
    assert manifest.is_inert_config_path(".codex-plugin/hooks.json") is False
    assert manifest.is_inert_config_path(".claude-plugin/.mcp.json") is False


def test_is_inert_config_path_rejects_plugin_shell_assets():
    assert manifest.is_inert_config_path(".claude-plugin/bin/ironmem-mcp.sh") is False
    assert manifest.is_inert_config_path(".claude-plugin/hooks/ironmem-hook.sh") is False


def test_is_docs_path_rejects_plugin_agent_markdown():
    assert manifest.is_docs_path(".claude-plugin/agents/code-reviewer.md") is False


def test_is_inert_config_path_still_matches_look_alike_backup_directory():
    # The look-alike negative: ".claude-plugin-backup/" must not trip the
    # plugin exclusion, so its .json file stays classified as ordinary
    # inert config -- proving the exclusion is a segment match, not a
    # substring match.
    assert manifest.is_inert_config_path(".claude-plugin-backup/x.json") is True


def test_classify_path_plugin_json_files_escalate():
    for path in (
        ".claude-plugin/plugin.json",
        ".codex-plugin/hooks.json",
        ".claude-plugin/.mcp.json",
    ):
        assert manifest.classify_path(path) == manifest.UNKNOWN


def test_classify_path_plugin_shell_assets_escalate():
    for path in (
        ".claude-plugin/bin/ironmem-mcp.sh",
        ".claude-plugin/hooks/ironmem-hook.sh",
    ):
        assert manifest.classify_path(path) == manifest.UNKNOWN


def test_classify_path_plugin_agent_markdown_escalates():
    assert manifest.classify_path(".claude-plugin/agents/code-reviewer.md") == manifest.UNKNOWN


def test_classify_path_plugin_backup_look_alike_stays_inert_config():
    assert manifest.classify_path(".claude-plugin-backup/x.json") == manifest.SURFACE_INERT_CONFIG


def test_resolve_gates_plugin_json_change_escalates_to_every_gate():
    # The regression this whole fix exists to close: a change touching only
    # .claude-plugin/plugin.json must select every phase-matching gate, not
    # just always-gates (there are none today) -- because
    # plugin_versions_match_cargo_toml (cargo test) reads this exact file.
    changes = manifest.ChangeSet(
        paths=(".claude-plugin/plugin.json",), unknown=False, reason=None
    )
    for phase in (manifest.PHASE_PRE_COMMIT, manifest.PHASE_PRE_PUSH):
        result = manifest.resolve_gates(phase, changes)
        expected = tuple(gate for gate in manifest.GATES if phase in gate.phases)
        assert result == expected


# --- Task 9: classify_path -- ordering protection for the new surface ----
#
# The whole-branch review's acceptance bullet: a `.sh` gate script must keep
# classifying hook_self_test even though it also matches
# is_inert_config_path's extension check, because SURFACE_HOOK_SELF_TEST is
# declared (and checked) before SURFACE_INERT_CONFIG.


def test_classify_path_install_git_hooks_sh_stays_hook_self_test():
    assert manifest.classify_path("scripts/install-git-hooks.sh") == manifest.SURFACE_HOOK_SELF_TEST


def test_classify_path_run_git_hook_py_stays_hook_self_test():
    assert manifest.classify_path("scripts/run_git_hook.py") == manifest.SURFACE_HOOK_SELF_TEST


def test_classify_path_test_run_git_hook_py_stays_hook_self_test():
    assert manifest.classify_path("scripts/test_run_git_hook.py") == manifest.SURFACE_HOOK_SELF_TEST


def test_classify_path_githooks_pre_commit_stays_hook_self_test():
    assert manifest.classify_path(".githooks/pre-commit") == manifest.SURFACE_HOOK_SELF_TEST


def test_classify_path_githooks_pre_push_stays_hook_self_test():
    assert manifest.classify_path(".githooks/pre-push") == manifest.SURFACE_HOOK_SELF_TEST


def test_classify_path_check_collab_turn_templates_stays_collab_protocol():
    assert (
        manifest.classify_path("scripts/check_collab_turn_templates.py")
        == manifest.SURFACE_COLLAB_PROTOCOL
    )


def test_classify_path_inert_config_json_file():
    assert manifest.classify_path("crates/ironmem/schema/example.json") == manifest.SURFACE_INERT_CONFIG


def test_classify_path_genuinely_unrecognized_path_still_escalates():
    # weird/thing.xyz matches no declared surface -- including the new
    # inert_config one -- so it must still classify UNKNOWN, and every
    # phase-matching gate must still be selected. The fail-closed property
    # must survive where it matters: an unrecognized extension is not
    # silently treated as inert.
    assert manifest.classify_path("weird/thing.xyz") == manifest.UNKNOWN
    changes = manifest.ChangeSet(paths=("weird/thing.xyz",), unknown=False, reason=None)
    for phase in (manifest.PHASE_PRE_COMMIT, manifest.PHASE_PRE_PUSH):
        result = manifest.resolve_gates(phase, changes)
        expected = tuple(gate for gate in manifest.GATES if phase in gate.phases)
        assert result == expected


# --- Task 2: classify_path -- near-misses classify UNKNOWN, not the surface
# they resemble -----------------------------------------------------------


def test_classify_path_near_miss_contests_is_not_collab_protocol():
    assert manifest.classify_path("contests/collab_turn_templates/example.txt") == manifest.UNKNOWN


def test_classify_path_near_miss_docsite_is_not_docs():
    assert manifest.classify_path("docsite/architecture.txt") == manifest.UNKNOWN


def test_classify_path_near_miss_src_backup_is_unknown():
    assert manifest.classify_path("src_backup/lib.py") == manifest.UNKNOWN


def test_classify_path_unrecognized_safe_shape_is_unknown():
    assert manifest.classify_path("notes.txt") == manifest.UNKNOWN


# --- Task 2: classify_path -- unsafe shapes classify UNKNOWN by rejection,
# never by crash and never by cleaning ------------------------------------


def test_classify_path_absolute_path_is_unknown():
    assert manifest.classify_path("/etc/passwd") == manifest.UNKNOWN


def test_classify_path_dotdot_segment_is_unknown():
    assert manifest.classify_path("scripts/../etc/passwd") == manifest.UNKNOWN


def test_classify_path_bare_dotdot_segment_is_unknown():
    assert manifest.classify_path("..") == manifest.UNKNOWN


def test_classify_path_nul_byte_is_unknown():
    assert manifest.classify_path("weird\x00path.md") == manifest.UNKNOWN


def test_classify_path_control_byte_is_unknown():
    assert manifest.classify_path("weird\x1bpath.md") == manifest.UNKNOWN


def test_classify_path_empty_string_is_unknown():
    assert manifest.classify_path("") == manifest.UNKNOWN


def test_classify_path_leading_dash_is_unknown():
    # Even though the extension would otherwise match the Rust surface, the
    # unsafe leading '-' shape check is rejected before surface matching.
    assert manifest.classify_path("-danger.rs") == manifest.UNKNOWN


def test_classify_path_non_str_int_is_unknown():
    assert manifest.classify_path(123) == manifest.UNKNOWN


def test_classify_path_non_str_none_is_unknown():
    assert manifest.classify_path(None) == manifest.UNKNOWN


def test_classify_path_non_str_list_is_unknown():
    assert manifest.classify_path(["scripts/run_git_hook.py"]) == manifest.UNKNOWN


def test_classify_path_never_raises_on_unsafe_shapes():
    # Escalation, not a crash: none of these may propagate an exception.
    unsafe_inputs = [
        "/etc/passwd",
        "..",
        "weird\x00path",
        "weird\x1bpath",
        "",
        "-rf",
        123,
        None,
        ["a"],
    ]
    for value in unsafe_inputs:
        assert manifest.classify_path(value) == manifest.UNKNOWN


# --- Task 2: paths are matched byte-exact -- no strip/case-fold/rewrite --


def test_classify_path_preserves_newline_space_and_non_ascii_segments():
    path = "docs/plan\n notes (β).md"
    assert manifest.classify_path(path) == manifest.SURFACE_DOCS
    # Segment-based matching operated on the real, unmodified bytes.
    assert path.split("/") == ["docs", "plan\n notes (β).md"]


def test_classify_path_preserves_carriage_return_like_newline():
    # A carriage return is exactly as legal inside a Git filename as a newline
    # is, and `-z` NUL framing makes both unambiguous at the source. Rejecting
    # \r while allowing \n was an asymmetry with no stated justification: it
    # escalated a correctly-classifiable docs path to a full gate run.
    assert manifest.classify_path("docs/plan\rnotes.md") == manifest.SURFACE_DOCS
    assert manifest.classify_path("docs/plan\r\nnotes.md") == manifest.SURFACE_DOCS


def test_classify_path_rejects_control_bytes_other_than_line_terminators():
    # The line-terminator allowance is exactly two bytes wide -- every other
    # control byte still escalates. Pinned so widening _ALLOWED_CONTROL_CHARS
    # to "all control bytes" fails here rather than passing silently.
    for codepoint in (0x00, 0x01, 0x08, 0x0B, 0x0C, 0x1B, 0x1F, 0x7F):
        path = f"docs/plan{chr(codepoint)}notes.md"
        assert manifest.classify_path(path) == manifest.UNKNOWN, f"U+{codepoint:04X} must escalate"


def test_classify_path_does_not_strip_whitespace_before_matching():
    # If classify_path stripped the path before matching, this would
    # collapse to the exact hook-self-test path and misclassify. Byte-exact
    # matching must leave it unrecognized instead.
    path = " scripts/run_git_hook.py"
    assert manifest.classify_path(path) == manifest.UNKNOWN


def test_classify_path_does_not_strip_trailing_newline_before_matching():
    path = "scripts/run_git_hook.py\n"
    assert manifest.classify_path(path) == manifest.UNKNOWN


# --- Task 3: resolve_gates -- unknown phase raises -----------------------


def test_resolve_gates_unknown_phase_raises():
    changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    with pytest.raises(ValueError):
        manifest.resolve_gates("typo-phase", changes)


def test_resolve_gates_empty_phase_string_raises():
    changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    with pytest.raises(ValueError):
        manifest.resolve_gates("", changes)


# --- Task 3: resolve_gates -- phase filtering tested both directions -----


def test_resolve_gates_pre_commit_excludes_pre_push_only_gate():
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert names == ["rust_fmt_check", "rust_clippy"]
    assert "rust_test" not in names


def test_resolve_gates_pre_push_excludes_pre_commit_only_gates():
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_PUSH, changes)]
    assert names == ["rust_test"]
    assert "rust_fmt_check" not in names
    assert "rust_clippy" not in names


# --- Task 3: resolve_gates -- docs inert, unknown dominates ---------------


def test_resolve_gates_docs_only_selects_no_gates():
    # No gate in today's manifest is marked always=True (see
    # test_manifest_no_gate_marked_always_yet), so an all-docs change with a
    # known shape selects nothing: DOCS is inert, not an escalation trigger.
    changes = manifest.ChangeSet(paths=("README.md",), unknown=False, reason=None)
    assert manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes) == ()
    assert manifest.resolve_gates(manifest.PHASE_PRE_PUSH, changes) == ()


def test_resolve_gates_docs_plus_code_path_does_not_skip():
    changes = manifest.ChangeSet(
        paths=("README.md", "crates/ironmem/src/hook.rs"), unknown=False, reason=None
    )
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert names == ["rust_fmt_check", "rust_clippy"]


# --- Task 9: resolve_gates -- inert_config is inert like docs -------------


def test_resolve_gates_inert_config_only_selects_no_gates():
    changes = manifest.ChangeSet(
        paths=(
            "crates/ironmem/schema/example.json",
            "site/index.html",
            "benchmarks/abeval/baseline_driver.py",
        ),
        unknown=False,
        reason=None,
    )
    assert manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes) == ()
    assert manifest.resolve_gates(manifest.PHASE_PRE_PUSH, changes) == ()


def test_resolve_gates_inert_config_plus_code_path_does_not_skip():
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/schema/example.json", "crates/ironmem/src/hook.rs"),
        unknown=False,
        reason=None,
    )
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert names == ["rust_fmt_check", "rust_clippy"]


def test_resolve_gates_inert_config_does_not_protect_sql_migration_from_escalation():
    # A staged crates/ironmem/migrations/*.sql change classifies UNKNOWN (not
    # inert_config -- see test_is_inert_config_path_rejects_sql_migration),
    # so it must still escalate to every gate for the phase.
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/migrations/001_init.sql",), unknown=False, reason=None
    )
    result = manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)
    expected = tuple(gate for gate in manifest.GATES if manifest.PHASE_PRE_COMMIT in gate.phases)
    assert result == expected


def test_resolve_gates_dashboard_html_change_escalates_to_rust_gates():
    # The dashboard embeds index.html with include_str!, so an HTML/XSS
    # regression is source-equivalent and must be tested, not classified as
    # inert static-site content.
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/dashboard/index.html",), unknown=False, reason=None
    )
    result = manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)
    expected = tuple(gate for gate in manifest.GATES if manifest.PHASE_PRE_COMMIT in gate.phases)
    assert result == expected


def test_resolve_gates_unrecognized_path_alone_runs_every_gate_for_phase():
    # "notes.txt" is a safe shape but classifies UNKNOWN (no declared surface
    # matches it). classify_path()'s UNKNOWN is the escalation signal, same
    # as changes.unknown=True -- it forces every phase-matching gate to run.
    changes = manifest.ChangeSet(paths=("notes.txt",), unknown=False, reason=None)
    result = manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)
    expected = tuple(gate for gate in manifest.GATES if manifest.PHASE_PRE_COMMIT in gate.phases)
    assert result == expected


def test_resolve_gates_docs_plus_unrecognized_runs_every_gate_unknown_dominates():
    changes = manifest.ChangeSet(
        paths=("README.md", "notes.txt"), unknown=False, reason=None
    )
    result = manifest.resolve_gates(manifest.PHASE_PRE_PUSH, changes)
    expected = tuple(gate for gate in manifest.GATES if manifest.PHASE_PRE_PUSH in gate.phases)
    assert result == expected


# --- Task 3: resolve_gates -- changes.unknown=True dominates paths --------


def test_resolve_gates_unknown_true_selects_full_phase_set_regardless_of_paths():
    changes = manifest.ChangeSet(paths=("README.md",), unknown=True, reason="git diff failed")
    result = manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)
    expected = tuple(gate for gate in manifest.GATES if manifest.PHASE_PRE_COMMIT in gate.phases)
    assert result == expected


def test_resolve_gates_unknown_true_with_empty_paths_selects_full_phase_set():
    changes = manifest.ChangeSet(paths=(), unknown=True, reason="malformed stdin")
    result = manifest.resolve_gates(manifest.PHASE_PRE_PUSH, changes)
    expected = tuple(gate for gate in manifest.GATES if manifest.PHASE_PRE_PUSH in gate.phases)
    assert result == expected


# --- Task 3: resolve_gates -- empty paths, unknown=False escalates nothing


def test_resolve_gates_empty_paths_unknown_false_selects_only_always_gates():
    changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    assert manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes) == ()
    assert manifest.resolve_gates(manifest.PHASE_PRE_PUSH, changes) == ()


# --- Task 3: resolve_gates -- the `gate.always` disjunct, exercised for real
#
# No gate in today's manifest sets always=True (see
# test_manifest_no_gate_marked_always_yet), so the assertion above only ever
# proves the degenerate "() == ()" case: it can't distinguish "always works"
# from "always is unreachable dead code". Inject a synthetic always-gate via
# monkeypatch (same technique as
# test_resolve_gates_dedupes_by_first_seen_not_by_sorting) to cover the
# branch with a gate that actually sets always=True.


def test_resolve_gates_gate_always_true_runs_with_empty_paths(monkeypatch):
    always_gate = manifest.Gate(
        name="synthetic_always_gate",
        argv=("true",),
        phases=frozenset({manifest.PHASE_PRE_COMMIT}),
        surfaces=frozenset(),
        always=True,
    )
    monkeypatch.setattr(manifest, "GATES", manifest.GATES + (always_gate,))
    changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    # Empty paths, unknown=False: nothing escalates. Only the always=True
    # gate fires -- every real manifest gate (always=False) is excluded.
    assert manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes) == (always_gate,)


# --- Task 3: resolve_gates -- output order is manifest order, invariant to
# input path order and duplicates; dedupe never changes the result ---------


def test_resolve_gates_output_order_is_manifest_order_invariant_to_input_order():
    forward = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs", "docs/COLLAB.md"),
        unknown=False,
        reason=None,
    )
    reordered = manifest.ChangeSet(
        paths=("docs/COLLAB.md", "crates/ironmem/src/hook.rs"),
        unknown=False,
        reason=None,
    )
    result_forward = manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, forward)
    result_reordered = manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, reordered)
    assert result_forward == result_reordered
    # Manifest order (see test_manifest_declaration_order_is_preserved), not
    # input order: collab_template_lint is declared before the rust gates.
    assert [gate.name for gate in result_forward] == [
        "collab_template_lint",
        "rust_fmt_check",
        "rust_clippy",
    ]


def test_resolve_gates_output_invariant_to_duplicate_paths():
    deduped = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    duplicated = manifest.ChangeSet(
        paths=(
            "crates/ironmem/src/hook.rs",
            "crates/ironmem/src/hook.rs",
            "crates/ironmem/src/hook.rs",
        ),
        unknown=False,
        reason=None,
    )
    assert manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, deduped) == manifest.resolve_gates(
        manifest.PHASE_PRE_COMMIT, duplicated
    )


def test_resolve_gates_dedupes_by_first_seen_not_by_sorting(monkeypatch):
    # Fixture-based monkeypatch (auto-restores on teardown): assert on
    # classify_path call order via a spy that wraps the real function,
    # proving resolve_gates visits each distinct path exactly once, in
    # first-seen order -- never a sorted order, which would reorder
    # "notes_b.txt" before "notes_a.txt".
    calls: list[str] = []
    real_classify_path = manifest.classify_path

    def spy(path):
        calls.append(path)
        return real_classify_path(path)

    changes = manifest.ChangeSet(
        paths=("notes_b.txt", "notes_a.txt", "notes_b.txt", "notes_a.txt"),
        unknown=False,
        reason=None,
    )
    monkeypatch.setattr(manifest, "classify_path", spy)
    manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)

    assert calls == ["notes_b.txt", "notes_a.txt"]


# --- Task 3: resolve_gates -- overlapping surfaces select each gate once --


def test_resolve_gates_overlapping_surfaces_select_each_gate_exactly_once():
    # Two different paths that both classify to SURFACE_HOOK_SELF_TEST must
    # not duplicate hook_self_test / hook_install_check in the result.
    changes = manifest.ChangeSet(
        paths=("scripts/run_git_hook.py", "scripts/install-git-hooks.sh"),
        unknown=False,
        reason=None,
    )
    result = manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)
    names = [gate.name for gate in result]
    assert names == ["hook_self_test", "hook_install_check"]
    assert len(names) == len(set(names))


# --- Task 3: resolve_gates -- returns a new tuple, never mutates inputs ---


def test_resolve_gates_returns_a_tuple():
    changes = manifest.ChangeSet(paths=(), unknown=True, reason="test")
    result = manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)
    assert isinstance(result, tuple)


def test_resolve_gates_does_not_mutate_changeset_paths():
    original_paths = ("crates/ironmem/src/hook.rs", "crates/ironmem/src/hook.rs")
    changes = manifest.ChangeSet(paths=original_paths, unknown=False, reason=None)
    manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)
    assert changes.paths == original_paths


# --- Task 3: resolve_gates -- parametrized per-gate-per-phase reachability -
#
# Derived from GATES itself, not a hardcoded list of gate names: a future
# gate appended to the manifest without an entry in
# _SURFACE_EXAMPLE_PATH_FOR_TEST below fails with a KeyError right here,
# rather than silently going unexercised.
#
# Parametrized over (gate, phase) pairs, not just gates: gate.phases is a
# frozenset, and CPython randomizes string hashing per process, so iterating
# a single `next(iter(gate.phases))` would exercise a different phase on
# different runs -- never both, never reproducibly, for any gate declaring
# more than one phase. Expanding to every declared pair removes that
# nondeterminism and strengthens the property being checked: every declared
# (gate, phase) pair must be reachable, not just one arbitrarily-chosen
# phase per gate. `sorted(gate.phases)` here is for deterministic *test
# parametrization ids*, not runtime gate ordering -- GATES itself is never
# sorted.
#
# SURFACE_DOCS and SURFACE_INERT_CONFIG are deliberately absent from the map
# below: no gate declares either today, and both are defined as inert (see
# test_resolve_gates_docs_only_selects_no_gates and
# test_resolve_gates_inert_config_only_selects_no_gates). If a future gate
# declared either one, this map must raise KeyError naming that gate
# immediately, not silently resolve to an inert-classified path that would
# make the always/escalate-only property look satisfied when it isn't.
_SURFACE_EXAMPLE_PATH_FOR_TEST = {
    manifest.SURFACE_RUST_WORKSPACE: "crates/ironmem/src/hook.rs",
    manifest.SURFACE_COLLAB_PROTOCOL: "docs/COLLAB.md",
    manifest.SURFACE_HOOK_SELF_TEST: "scripts/run_git_hook.py",
}

_GATE_PHASE_PARAMS_FOR_TEST = [
    (gate, phase) for gate in manifest.GATES for phase in sorted(gate.phases)
]


@pytest.mark.parametrize(
    "gate, phase",
    _GATE_PHASE_PARAMS_FOR_TEST,
    ids=[f"{gate.name}-{phase}" for gate, phase in _GATE_PHASE_PARAMS_FOR_TEST],
)
def test_resolve_gates_reaches_every_manifest_gate(gate, phase):
    if gate.always:
        changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
        assert gate in manifest.resolve_gates(phase, changes)
        return
    # Iterate every declared surface, not just one: a future gate declaring
    # two surfaces -- one mapped here, one not -- must raise KeyError
    # unconditionally (not on a hash-order coin flip), and must be proven
    # reachable from each surface it declares, not just an arbitrary one.
    for surface_id in gate.surfaces:
        path = _SURFACE_EXAMPLE_PATH_FOR_TEST[surface_id]
        changes = manifest.ChangeSet(paths=(path,), unknown=False, reason=None)
        assert gate in manifest.resolve_gates(phase, changes)


# --- Task 4: collection layer -- Git to ChangeSet, fail-closed ------------
#
# No test in this section invokes real Git: every `subprocess.run` call is
# replaced by `_FakeGitRun`, keyed on the exact argv tuple that follows
# "git". An unanticipated call raises KeyError (a loud test failure), which
# doubles as proof that each test drives an exact, intentional sequence of
# Git invocations -- never a superset or a silently-skipped one.

SHA_A = "a" * 40
SHA_B = "b" * 40
SHA_C = "c" * 40

# The exact collection-layer argv tuples, spelled once. `--no-renames` is
# load-bearing (see run_git_hook.py's collection-layer comment); _FakeGitRun
# raises KeyError on any argv it was not primed for, so these constants still
# pin the invocation byte-for-byte.
_PRE_COMMIT_DIFF = ("diff", "--cached", "--name-only", "--no-renames", "-z")


def _range_diff(base, head):
    return ("diff", "--name-only", "--no-renames", "-z", f"{base}..{head}")


def _root_diff(sha):
    return (
        "diff-tree",
        "--root",
        "--no-commit-id",
        "--name-only",
        "--no-renames",
        "-z",
        "-r",
        sha,
    )


# Git's all-zero null object ids, derived from the module's own supported
# object-id lengths rather than from a SHA-1-only literal: the production code
# recognizes both, so the tests must exercise the same vocabulary.
ZERO_SHA_1 = "0" * min(collect._SHA_LENGTHS)
ZERO_SHA_256 = "0" * max(collect._SHA_LENGTHS)


class _FakeGitRun:
    """Stand-in for `subprocess.run` used by every Task 4 test. Never shells
    out. `responses` maps an argv tuple (everything after "git") to either
    an `(returncode, stdout)` pair or an exception instance to raise,
    simulating a genuine subprocess-level failure (git missing, output
    undecodable, ...).
    """

    def __init__(self, responses):
        self.responses = responses
        self.calls: list[tuple[str, ...]] = []

    def __call__(self, cmd, **kwargs):
        assert cmd[0] == "git"
        assert kwargs.get("cwd") == runtime.ROOT
        assert kwargs.get("text") is True
        assert kwargs.get("capture_output") is True
        assert kwargs.get("check") is False
        assert "shell" not in kwargs
        args = tuple(cmd[1:])
        self.calls.append(args)
        outcome = self.responses[args]
        if isinstance(outcome, BaseException):
            raise outcome
        returncode, stdout = outcome
        return subprocess.CompletedProcess(cmd, returncode, stdout=stdout, stderr="")


def _pre_push_line(local_ref, local_sha, remote_ref, remote_sha):
    return f"{local_ref} {local_sha} {remote_ref} {remote_sha}"


# --- _split_nul -- byte-exact NUL-field splitting --------------------------


def test_split_nul_empty_output_is_no_paths():
    assert collect._split_nul("") == ()


def test_split_nul_drops_only_trailing_empty_field():
    assert collect._split_nul("a.py\0b.py\0") == ("a.py", "b.py")


def test_split_nul_preserves_interior_bytes_no_strip():
    # Leading/trailing whitespace and an embedded newline inside a field
    # must survive untouched -- -z framing removal is not path mutation.
    assert collect._split_nul(" a.py \0b\n.py\0") == (" a.py ", "b\n.py")


# --- _is_hex_sha -- sha validation (not a path, .strip()/case rules N/A) --


def test_is_hex_sha_accepts_full_hex_sha():
    assert collect._is_hex_sha(SHA_A) is True


def test_is_hex_sha_rejects_empty_string():
    assert collect._is_hex_sha("") is False


def test_is_hex_sha_rejects_non_hex_characters():
    assert collect._is_hex_sha("z" * 40) is False


def test_is_hex_sha_rejects_short_hex_run():
    # "abc" is hex-shaped but far shorter than a real Git object id (40 or
    # 64 hex chars). Accepting any positive-length hex run would make this
    # guard a formality rather than a load-bearing malformed-stdin check.
    assert collect._is_hex_sha("abc") is False


def test_is_hex_sha_accepts_sha256_length():
    assert collect._is_hex_sha("a" * 64) is True


def test_is_hex_sha_rejects_length_between_40_and_64():
    assert collect._is_hex_sha("a" * 50) is False


def test_is_zero_sha_accepts_sha1_and_sha256_null_object_ids():
    assert collect._is_zero_sha(ZERO_SHA_1) is True
    assert collect._is_zero_sha(ZERO_SHA_256) is True


def test_is_zero_sha_rejects_non_null_or_invalid_object_ids():
    assert collect._is_zero_sha(SHA_A) is False
    assert collect._is_zero_sha("0" * 50) is False


# --- _parse_pre_push_line ---------------------------------------------------


def test_parse_pre_push_line_valid_four_fields():
    line = _pre_push_line("refs/heads/a", SHA_A, "refs/heads/a", SHA_B)
    assert collect._parse_pre_push_line(line) == ("refs/heads/a", SHA_A, "refs/heads/a", SHA_B)


def test_parse_pre_push_line_wrong_field_count_is_none():
    assert collect._parse_pre_push_line("refs/heads/a onlytwo") is None


# --- _run_git -- fail-closed boundary itself must never raise --------------


def test_run_git_guards_empty_args_on_subprocess_failure(monkeypatch):
    # `_run_git(())` -- an empty args tuple -- must not raise IndexError from
    # inside its own except-block while building `reason`; that would let an
    # exception escape the one boundary that exists to convert Git
    # subprocess failures into a structured, non-raising signal.
    def raiser(cmd, **kwargs):
        raise OSError("boom")

    monkeypatch.setattr(subprocess, "run", raiser)
    ok, returncode, stdout, reason = collect._run_git(())
    assert ok is False
    assert returncode == -1
    assert stdout == ""
    assert reason
    assert "<no-args>" in reason
    # CONTRACT CHANGE: `reason` now carries the exception message as well as
    # its class name (see _run_git's docstring). The class name alone made
    # PermissionError/OSError/UnicodeDecodeError indistinguishable in a
    # failure report. The invariant that still holds -- and is what the
    # earlier "message excluded" assertion was really protecting -- is that
    # `reason` never carries the subprocess's captured output or the
    # environment, and that no caller passes a remote URL in the argv.
    assert "OSError: boom" in reason


# --- collect_pre_commit_changes ---------------------------------------------


def test_collect_pre_commit_changes_success(monkeypatch):
    fake = _FakeGitRun({_PRE_COMMIT_DIFF: (0, "a.py\0b/c.txt\0")})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_commit_changes()
    assert changes == manifest.ChangeSet(paths=("a.py", "b/c.txt"), unknown=False, reason=None)
    assert fake.calls == [_PRE_COMMIT_DIFF]


def test_collect_pre_commit_changes_no_staged_files_is_not_unknown(monkeypatch):
    fake = _FakeGitRun({_PRE_COMMIT_DIFF: (0, "")})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_commit_changes()
    # Empty paths + unknown=False must mean "genuinely no changes", never
    # "collection broke".
    assert changes == manifest.ChangeSet(paths=(), unknown=False, reason=None)


def test_collect_pre_commit_changes_nonzero_exit_is_unknown(monkeypatch):
    fake = _FakeGitRun({_PRE_COMMIT_DIFF: (128, "")})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_commit_changes()
    assert changes.paths == ()
    assert changes.unknown is True
    assert changes.reason


def test_collect_pre_commit_changes_subprocess_failure_is_unknown_never_raises(monkeypatch):
    fake = _FakeGitRun(
        {_PRE_COMMIT_DIFF: FileNotFoundError("git: command not found")}
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_commit_changes()
    assert changes.paths == ()
    assert changes.unknown is True
    assert changes.reason
    # CONTRACT CHANGE: the exception message is now propagated into `reason`
    # (see test_run_git_guards_empty_args_on_subprocess_failure); the
    # subprocess's captured output still never is.
    assert "FileNotFoundError: git: command not found" in changes.reason


def test_collect_pre_commit_changes_preserves_byte_exact_paths(monkeypatch):
    weird = "docs/plan\n notes (β).md"
    fake = _FakeGitRun({_PRE_COMMIT_DIFF: (0, f"{weird}\0")})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_commit_changes()
    assert changes.paths == (weird,)


def test_collect_pre_commit_changes_no_diff_filter_flag(monkeypatch):
    # Pins that collect_pre_commit_changes() never passes --diff-filter: the
    # exact argv it must invoke is asserted by _FakeGitRun's KeyError-on-
    # unanticipated-call behavior (see class docstring above) -- any
    # additional flag, including a reintroduced --diff-filter, would make
    # this call miss the fake's response table and fail loudly.
    fake = _FakeGitRun(
        {_PRE_COMMIT_DIFF: (0, "crates/ironmem/src/deleted.rs\0")}
    )
    monkeypatch.setattr(subprocess, "run", fake)
    collect.collect_pre_commit_changes()
    assert fake.calls == [_PRE_COMMIT_DIFF]


def test_staged_deletion_of_rust_path_is_collected_and_selects_rust_gates(monkeypatch):
    # The human-ratified behavior change: staged deletions reach gate
    # selection because --diff-filter=ACMRTUXB was deliberately dropped (see
    # the comment at collect_pre_commit_changes()). `git diff --name-only`
    # reports a deleted path exactly like any other changed path -- there is
    # no separate "deleted" marker in `-z --name-only` output -- so a fake
    # response containing only the path is a faithful stand-in for a staged
    # deletion of that path. This pins the ratified behavior: the deletion
    # is collected (not dropped) and it selects the Rust gates for
    # pre-commit.
    fake = _FakeGitRun(
        {_PRE_COMMIT_DIFF: (0, "crates/ironmem/src/deleted.rs\0")}
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_commit_changes()
    assert changes == manifest.ChangeSet(
        paths=("crates/ironmem/src/deleted.rs",), unknown=False, reason=None
    )
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert "rust_fmt_check" in names
    assert "rust_clippy" in names


# --- _run_git reason detail and timeout ------------------------------------


def test_run_git_reason_includes_full_argv_not_just_the_subcommand():
    args = _PRE_COMMIT_DIFF
    fake = _FakeGitRun({args: OSError("boom")})
    ok, _rc, _stdout, reason = _call_with_fake_run(fake, collect._run_git, args)
    assert ok is False
    for token in args:
        assert token in reason


def test_run_git_reason_includes_the_exception_message():
    # PermissionError / OSError / UnicodeDecodeError are indistinguishable
    # when only the class name is recorded.
    args = _PRE_COMMIT_DIFF
    fake = _FakeGitRun({args: PermissionError("permission denied: /usr/bin/git")})
    _ok, _rc, _stdout, reason = _call_with_fake_run(fake, collect._run_git, args)
    assert "PermissionError" in reason
    assert "permission denied: /usr/bin/git" in reason


def test_run_git_passes_a_timeout():
    args = _PRE_COMMIT_DIFF
    recorder = _KwargRecordingRun()
    _call_with_fake_run(recorder, collect._run_git, args)
    assert recorder.kwargs[0]["timeout"] == runtime._GIT_TIMEOUT_SECONDS


def test_legacy_git_helper_passes_a_timeout():
    recorder = _KwargRecordingRun()
    _call_with_fake_run(recorder, collect.git, ["rev-parse", "--verify", "@{u}"], check=False)
    assert recorder.kwargs[0]["timeout"] == runtime._GIT_TIMEOUT_SECONDS


def test_collect_pre_commit_changes_timeout_is_fail_closed(monkeypatch):
    # A hung git must escalate, never present as "nothing staged".
    args = _PRE_COMMIT_DIFF
    fake = _FakeGitRun({args: subprocess.TimeoutExpired(["git", *args], 60)})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_commit_changes()
    assert changes.unknown is True
    assert "TimeoutExpired" in changes.reason


class _KwargRecordingRun:
    """`subprocess.run` stand-in recording every call's kwargs."""

    def __init__(self):
        self.kwargs: list[dict] = []

    def __call__(self, cmd, **kwargs):
        self.kwargs.append(kwargs)
        return subprocess.CompletedProcess(cmd, 0, stdout="", stderr="")


def _call_with_fake_run(fake, func, *args, **kwargs):
    real = subprocess.run
    subprocess.run = fake
    try:
        return func(*args, **kwargs)
    finally:
        subprocess.run = real


# --- collection layer runs with a scrubbed GIT_* env ------------------------
#
# Verified empirically before these tests were written: with `GIT_DIR` exported
# at repo A and the process cwd inside repo B, `git diff --cached --name-only`
# reports repo A's staged paths and exits 0. Because it exits 0, the
# collection layer's fail-closed `unknown=True` is never set -- the hook
# silently gates on a different repository's change set. That is a fail-OPEN
# bug, strictly worse than the outbound leak `_scrub_git_env` already targeted
# at the execution boundary.

_REPO_REDIRECTING_VARS = (
    "GIT_DIR",
    "GIT_INDEX_FILE",
    "GIT_WORK_TREE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_COMMON_DIR",
    "GIT_NAMESPACE",
    "GIT_CONFIG_COUNT",
    "GIT_CONFIG_GLOBAL",
)


class _EnvRecordingGitRun:
    """`subprocess.run` stand-in that records the `env=` kwarg of every call."""

    def __init__(self, stdout=""):
        self.stdout = stdout
        self.envs: list[object] = []

    def __call__(self, cmd, **kwargs):
        self.envs.append(kwargs.get("env"))
        return subprocess.CompletedProcess(cmd, 0, stdout=self.stdout, stderr="")


def _poison_git_env(monkeypatch):
    for name in _REPO_REDIRECTING_VARS:
        monkeypatch.setenv(name, "/somewhere/else")
    monkeypatch.setenv("PATH", "/usr/bin")


def test_collect_pre_commit_changes_scrubs_repo_redirecting_git_env(monkeypatch):
    _poison_git_env(monkeypatch)
    fake = _EnvRecordingGitRun("a.py\0")
    monkeypatch.setattr(subprocess, "run", fake)
    collect.collect_pre_commit_changes()
    assert fake.envs, "collection layer made no subprocess call"
    for env in fake.envs:
        assert env is not None, "collection layer inherited the ambient env"
        for name in _REPO_REDIRECTING_VARS:
            assert name not in env
        assert env["PATH"] == "/usr/bin"


def test_collect_pre_push_changes_scrubs_repo_redirecting_git_env(monkeypatch):
    _poison_git_env(monkeypatch)
    stdin = _pre_push_line("refs/heads/feature", SHA_B, "refs/heads/feature", SHA_A) + "\n"
    fake = _EnvRecordingGitRun("a.py\0")
    monkeypatch.setattr(subprocess, "run", fake)
    collect.collect_pre_push_changes(stdin)
    assert fake.envs
    for env in fake.envs:
        assert env is not None
        for name in _REPO_REDIRECTING_VARS:
            assert name not in env


def test_legacy_git_helper_scrubs_repo_redirecting_git_env(monkeypatch):
    # The manual `@{u}` fallback still goes through the un-hardened `git()`
    # helper; its subprocess env must be scrubbed too, or a manual pre-push
    # invocation inherits the same redirection.
    _poison_git_env(monkeypatch)
    fake = _EnvRecordingGitRun("")
    monkeypatch.setattr(subprocess, "run", fake)
    collect.git(["rev-parse", "--verify", "@{u}"], check=False)
    assert fake.envs
    for env in fake.envs:
        assert env is not None
        for name in _REPO_REDIRECTING_VARS:
            assert name not in env


def test_collection_layer_env_keeps_ssh_auth_variables(monkeypatch):
    # The scrub is the same allowlist the execution layer uses: variables that
    # configure *how* Git authenticates survive; only repo-redirecting ones go.
    _poison_git_env(monkeypatch)
    monkeypatch.setenv("GIT_SSH_COMMAND", "ssh -i /key")
    fake = _EnvRecordingGitRun("")
    monkeypatch.setattr(subprocess, "run", fake)
    collect.collect_pre_commit_changes()
    assert fake.envs[0]["GIT_SSH_COMMAND"] == "ssh -i /key"


def test_collect_pre_commit_changes_disables_rename_detection(monkeypatch):
    # Regression: `git diff --name-only` has rename detection ON by default
    # and prints ONLY the destination path. Renaming a gated file to an inert
    # destination (`crates/.../foo.rs` -> `docs/foo.md`) would therefore yield
    # a ChangeSet containing only the inert path, classify DOCS, select no
    # gates, and exit 0 -- fmt/clippy never running on a workspace that may no
    # longer compile. `--no-renames` makes Git report both sides.
    fake = _FakeGitRun(
        {
            _PRE_COMMIT_DIFF: (
                0,
                "docs/foo.md\0crates/ironmem/src/foo.rs\0",
            )
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_commit_changes()
    assert "crates/ironmem/src/foo.rs" in changes.paths
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_COMMIT, changes)]
    assert "rust_fmt_check" in names
    assert "rust_clippy" in names


# --- collect_pre_push_changes -- happy paths --------------------------------


def test_collect_pre_push_changes_single_update(monkeypatch):
    stdin = _pre_push_line("refs/heads/feature", SHA_B, "refs/heads/feature", SHA_A) + "\n"
    fake = _FakeGitRun({_range_diff(SHA_A, SHA_B): (0, "x.py\0y.py\0")})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes == manifest.ChangeSet(paths=("x.py", "y.py"), unknown=False, reason=None)


def test_collect_pre_push_changes_disables_rename_detection(monkeypatch):
    # Same fail-open as the pre-commit collector, on the range diff: without
    # `--no-renames` a `.rs` -> `.md` rename pushed to the remote would report
    # only the inert destination and skip `rust_test`.
    stdin = _pre_push_line("refs/heads/feature", SHA_B, "refs/heads/feature", SHA_A) + "\n"
    fake = _FakeGitRun(
        {
            _range_diff(SHA_A, SHA_B): (
                0,
                "docs/foo.md\0crates/ironmem/src/foo.rs\0",
            )
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert "crates/ironmem/src/foo.rs" in changes.paths
    names = [gate.name for gate in manifest.resolve_gates(manifest.PHASE_PRE_PUSH, changes)]
    assert "rust_test" in names


def test_collect_pre_push_branch_creation_diff_tree_disables_rename_detection(monkeypatch):
    # The branch-creation root-diff path (`git diff-tree`) has the same
    # default rename detection and needs the same flag.
    stdin = _pre_push_line("refs/heads/new", SHA_B, "refs/heads/new", ZERO_SHA_1) + "\n"
    fake = _FakeGitRun(
        {
            ("symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"): (1, ""),
            ("merge-base", SHA_B, "origin/main"): (1, ""),
            ("merge-base", SHA_B, "origin/master"): (1, ""),
            ("merge-base", SHA_B, "main"): (1, ""),
            ("merge-base", SHA_B, "master"): (1, ""),
            _root_diff(SHA_B): (0, "crates/ironmem/src/foo.rs\0"),
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes.paths == ("crates/ironmem/src/foo.rs",)


def test_collect_pre_push_changes_multi_ref_dedupes_first_seen(monkeypatch):
    stdin = (
        "\n".join(
            [
                _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A),
                _pre_push_line("refs/heads/b", SHA_C, "refs/heads/b", SHA_A),
            ]
        )
        + "\n"
    )
    fake = _FakeGitRun(
        {
            _range_diff(SHA_A, SHA_B): (0, "x.py\0shared.py\0"),
            _range_diff(SHA_A, SHA_C): (0, "shared.py\0z.py\0"),
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes.paths == ("x.py", "shared.py", "z.py")
    assert changes.unknown is False


def test_collect_pre_push_changes_skips_deletion_ref(monkeypatch):
    stdin = _pre_push_line("refs/heads/gone", ZERO_SHA_1, "refs/heads/gone", SHA_A) + "\n"
    fake = _FakeGitRun({})  # no git diff call should happen at all
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes == manifest.ChangeSet(paths=(), unknown=False, reason=None)
    assert fake.calls == []


def test_collect_pre_push_changes_skips_sha256_deletion_ref(monkeypatch):
    sha256_remote = "a" * 64
    stdin = _pre_push_line("refs/heads/gone", ZERO_SHA_256, "refs/heads/gone", sha256_remote) + "\n"
    fake = _FakeGitRun({})  # no git diff call should happen at all
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes == manifest.ChangeSet(paths=(), unknown=False, reason=None)
    assert fake.calls == []


def test_collect_pre_push_changes_branch_creation_uses_default_base(monkeypatch):
    stdin = _pre_push_line("refs/heads/new", SHA_B, "refs/heads/new", ZERO_SHA_1) + "\n"
    fake = _FakeGitRun(
        {
            ("symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"): (
                0,
                "refs/remotes/origin/main\n",
            ),
            ("merge-base", SHA_B, "refs/remotes/origin/main"): (0, SHA_A + "\n"),
            _range_diff(SHA_A, SHA_B): (0, "new_file.py\0"),
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes == manifest.ChangeSet(paths=("new_file.py",), unknown=False, reason=None)


def test_collect_pre_push_changes_sha256_branch_creation_uses_default_base(monkeypatch):
    sha256_base = "a" * 64
    sha256_local = "b" * 64
    stdin = _pre_push_line("refs/heads/new", sha256_local, "refs/heads/new", ZERO_SHA_256) + "\n"
    fake = _FakeGitRun(
        {
            ("symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"): (
                0,
                "refs/remotes/origin/main\n",
            ),
            ("merge-base", sha256_local, "refs/remotes/origin/main"): (0, sha256_base + "\n"),
            _range_diff(sha256_base, sha256_local): (0, "new_file.py\0"),
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes == manifest.ChangeSet(paths=("new_file.py",), unknown=False, reason=None)


def test_collect_pre_push_changes_missing_upstream_falls_back_to_root_diff(monkeypatch):
    stdin = _pre_push_line("refs/heads/new", SHA_B, "refs/heads/new", ZERO_SHA_1) + "\n"
    fake = _FakeGitRun(
        {
            ("symbolic-ref", "--quiet", "refs/remotes/origin/HEAD"): (1, ""),
            ("merge-base", SHA_B, "origin/main"): (1, ""),
            ("merge-base", SHA_B, "origin/master"): (1, ""),
            ("merge-base", SHA_B, "main"): (1, ""),
            ("merge-base", SHA_B, "master"): (1, ""),
            _root_diff(SHA_B): (
                0,
                "root.py\0",
            ),
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes == manifest.ChangeSet(paths=("root.py",), unknown=False, reason=None)


def test_collect_pre_push_changes_empty_stdin_is_not_unknown(monkeypatch):
    fake = _FakeGitRun({})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes("")
    assert changes == manifest.ChangeSet(paths=(), unknown=False, reason=None)
    assert fake.calls == []


# --- collect_pre_push_changes -- fail-closed: Git failures ------------------


def test_collect_pre_push_changes_git_failure_mid_batch_preserves_prior_paths(monkeypatch):
    stdin = (
        "\n".join(
            [
                _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A),
                _pre_push_line("refs/heads/b", SHA_C, "refs/heads/b", SHA_A),
            ]
        )
        + "\n"
    )
    fake = _FakeGitRun(
        {
            _range_diff(SHA_A, SHA_B): (0, "x.py\0"),
            _range_diff(SHA_A, SHA_C): (128, ""),
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    # Whatever was collected before the failure is preserved -- never wiped
    # back to an empty tuple.
    assert changes.paths == ("x.py",)
    assert changes.unknown is True
    assert changes.reason


def test_collect_pre_push_changes_subprocess_failure_is_unknown_never_raises(monkeypatch):
    stdin = _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A) + "\n"
    fake = _FakeGitRun({_range_diff(SHA_A, SHA_B): OSError("boom")})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes.paths == ()
    assert changes.unknown is True
    assert changes.reason
    assert "OSError: boom" in changes.reason


# --- collect_pre_push_changes -- fail-closed: malformed stdin ---------------


@pytest.mark.parametrize(
    "stdin",
    [
        "refs/heads/a onlythreefields\n",
        f"refs/heads/a zzzznothex refs/heads/a {SHA_A}\n",
        f"refs/heads/a {SHA_A} refs/heads/a zzzznothex\n",
        "☠️ not a valid pre-push line at all\n",
    ],
    ids=["wrong-field-count", "non-hex-local-sha", "non-hex-remote-sha", "junk"],
)
def test_collect_pre_push_changes_malformed_stdin_is_unknown_never_raises(monkeypatch, stdin):
    fake = _FakeGitRun({})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = collect.collect_pre_push_changes(stdin)
    assert changes.unknown is True
    assert changes.reason
    # Malformed input is rejected before any Git call is attempted.
    assert fake.calls == []


class _StdinStub:
    """Minimal stand-in for sys.stdin exposing only the .read() that
    main("pre-push") calls -- avoids touching the real process stdin in
    tests.
    """

    def __init__(self, text: str) -> None:
        self._text = text

    def read(self) -> str:
        return self._text


# --- Task 6: legacy retirement -- these symbols must be gone, not merely
# unused --------------------------------------------------------------------


def test_legacy_pre_task6_functions_are_removed():
    for name in (
        "run_pre_commit",
        "run_pre_push",
        "gate_summary",
        "run",
        "staged_paths",
        "pushed_paths",
    ):
        for module in (cli, manifest, collect, execute, runtime):
            assert not hasattr(module, name), (
                f"{name} should have been retired in Task 6, found on "
                f"{module.__name__}"
            )


# --- Task 5: execution layer -- hardened subprocess contract --------------
#
# No test in this section invokes a real gate command (cargo/pytest/bash are
# slow and side-effecting). Every `subprocess.run` call is replaced by
# `_FakeGateRun`, keyed on the exact argv *list* execute_gates passes --
# distinct from `_FakeGitRun` above, which asserts `cmd[0] == "git"` and
# would reject a gate invocation outright.


class _FakeGateRun:
    """Stand-in for `subprocess.run` used by every Task 5 test. Never shells
    out and never runs a real gate. `responses` maps an argv tuple to either
    the returncode that call should yield, or an exception instance to raise
    (simulating exec-time failures such as a missing gate binary). An
    unanticipated call raises KeyError, proving each test drives an exact,
    intentional sequence of gate invocations.
    """

    def __init__(self, responses):
        self.responses = responses
        self.calls: list[tuple[list, dict]] = []

    def __call__(self, cmd, **kwargs):
        self.calls.append((list(cmd), kwargs))
        key = tuple(cmd)
        outcome = self.responses[key]
        if isinstance(outcome, BaseException):
            raise outcome
        return subprocess.CompletedProcess(cmd, outcome)


def _only_rust_changes():
    return manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )


# --- execute_gates -- runs only resolver-selected gates, in manifest order -


def test_execute_gates_runs_only_selected_gates_in_manifest_order(monkeypatch):
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): 0,
            (
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ): 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, _only_rust_changes())
    assert rc == 0
    assert [cmd for cmd, _kwargs in fake.calls] == [
        ["cargo", "fmt", "--all", "--", "--check"],
        [
            "cargo",
            "clippy",
            "--workspace",
            "--all-targets",
            "--all-features",
            "--",
            "-D",
            "warnings",
        ],
    ]


def test_execute_gates_returns_zero_when_nothing_selected(monkeypatch):
    fake = _FakeGateRun({})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = manifest.ChangeSet(paths=("README.md",), unknown=False, reason=None)
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    assert rc == 0
    assert fake.calls == []


# --- execute_gates -- the hardened subprocess contract itself -------------


def test_execute_gates_calls_subprocess_run_with_exact_contract(monkeypatch):
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): 0,
            (
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ): 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    execute.execute_gates(manifest.PHASE_PRE_COMMIT, _only_rust_changes())
    cmd, kwargs = fake.calls[0]
    assert cmd == ["cargo", "fmt", "--all", "--", "--check"]
    assert kwargs.get("shell") is False
    assert kwargs.get("cwd") == runtime.ROOT
    assert kwargs.get("check") is False
    assert "env" in kwargs


# --- execute_gates -- stop-at-first-failure, propagate exact exit code ----


def test_execute_gates_stops_at_first_failure_and_propagates_exit_code(monkeypatch):
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): 3,
            (
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ): 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, _only_rust_changes())
    assert rc == 3
    # rust_clippy must never be invoked once rust_fmt_check has failed.
    assert [cmd for cmd, _kwargs in fake.calls] == [
        ["cargo", "fmt", "--all", "--", "--check"]
    ]


# --- execute_gates -- one deterministic line per gate: run/skip/fail ------


def test_execute_gates_prints_run_line_for_selected_gate(monkeypatch, capsys):
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): 0})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    # Restrict to a single-gate manifest slice so this test only proves the
    # run-line, not interactions with the rest of the real manifest.
    fmt_gate = next(gate for gate in manifest.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(manifest, "GATES", (fmt_gate,))
    execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    # Exact output, not a substring check -- the task calls the per-gate
    # format deterministic, so the test should pin the literal bytes rather
    # than accept e.g. "rerun" or a stray "7" anywhere in unrelated output.
    # The trailing completion line (Task 9 Part C) restores the "the run
    # completed intentionally" statement the pre-Task-6 runner printed
    # (`[pre-commit] staged files: N`) and this refactor had dropped.
    assert out == "[git-hook] rust_fmt_check: run\n[git-hook] pre-commit: 1 gate(s) run, 0 failed\n"


def test_execute_gates_prints_skip_line_with_surfaces_not_touched(monkeypatch, capsys):
    fake = _FakeGateRun({})
    monkeypatch.setattr(subprocess, "run", fake)
    collab_gate = next(
        gate for gate in manifest.GATES if gate.name == "collab_template_lint"
    )
    monkeypatch.setattr(manifest, "GATES", (collab_gate,))
    changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    # Exact output, not three substring checks. The per-gate format is a
    # deterministic contract, and the substring form passed against output
    # that merely mentioned the gate and the word "skip" anywhere -- it could
    # not tell `skip (collab_protocol)` from `skip (collab_protocol,docs)` or
    # from a skip line emitted for some other gate entirely.
    assert out == (
        "[git-hook] collab_template_lint: skip (collab_protocol)\n"
        "[git-hook] pre-commit: no local gates required\n"
    )
    assert fake.calls == []


def test_execute_gates_skip_line_lists_surfaces_deterministically(monkeypatch, capsys):
    # NOTE: kept for its original within-process-rerun angle, but every gate
    # in today's manifest (including hook_self_test, used here) declares
    # exactly one surface, so this test cannot actually distinguish ordered
    # output from raw frozenset iteration -- with one element the two are
    # byte-identical by construction. It would pass identically against the
    # bug it was meant to catch. The real ordering-property coverage is
    # test__ordered_surfaces_returns_declaration_order below, which drives
    # _ordered_surfaces directly with a genuinely multi-element frozenset.
    fake = _FakeGateRun({})
    monkeypatch.setattr(subprocess, "run", fake)
    hook_gate = next(gate for gate in manifest.GATES if gate.name == "hook_self_test")
    monkeypatch.setattr(manifest, "GATES", (hook_gate,))
    changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    first = capsys.readouterr().out
    execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    second = capsys.readouterr().out
    assert first == second


# --- _ordered_surfaces -- the real ordering-property coverage -------------


def test__ordered_surfaces_returns_declaration_order():
    # SURFACES declares SURFACE_RUST_WORKSPACE before SURFACE_COLLAB_PROTOCOL
    # before SURFACE_HOOK_SELF_TEST before SURFACE_DOCS (see the SURFACES
    # MappingProxyType above). Passing the two later-declared ids in reverse
    # frozenset-construction order and asserting the exact declaration-order
    # tuple back is the property test_execute_gates_skip_line_lists_
    # surfaces_deterministically cannot provide with a single-element input:
    # this fails if _ordered_surfaces ever regresses to raw frozenset
    # iteration (hash-randomized, not declaration order).
    surface_ids = frozenset({manifest.SURFACE_DOCS, manifest.SURFACE_HOOK_SELF_TEST})
    assert manifest._ordered_surfaces(surface_ids) == (
        manifest.SURFACE_HOOK_SELF_TEST,
        manifest.SURFACE_DOCS,
    )

    all_surfaces = frozenset(
        {
            manifest.SURFACE_DOCS,
            manifest.SURFACE_COLLAB_PROTOCOL,
            manifest.SURFACE_HOOK_SELF_TEST,
            manifest.SURFACE_RUST_WORKSPACE,
        }
    )
    assert manifest._ordered_surfaces(all_surfaces) == (
        manifest.SURFACE_RUST_WORKSPACE,
        manifest.SURFACE_COLLAB_PROTOCOL,
        manifest.SURFACE_HOOK_SELF_TEST,
        manifest.SURFACE_DOCS,
    )


def test_execute_gates_prints_fail_line_with_exit_code(monkeypatch, capsys):
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): 7})
    monkeypatch.setattr(subprocess, "run", fake)
    fmt_gate = next(gate for gate in manifest.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(manifest, "GATES", (fmt_gate,))
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    assert rc == 7
    out = capsys.readouterr().out
    # Exact output. `assert "7" in out` was the weakest check in the suite:
    # any exit code containing the digit 7, any gate name, or any unrelated
    # line carrying a 7 satisfied it. Pin the literal bytes instead -- and
    # note there is deliberately NO trailing completion line here, because a
    # run that stopped at a non-zero exit did not complete.
    assert out == "[git-hook] rust_fmt_check: run\n[git-hook] rust_fmt_check: fail (7)\n"


def test_execute_gates_normalizes_negative_returncode_from_signal_kill(monkeypatch, capsys):
    # A signal-killed gate (e.g. SIGKILL) reports a negative returncode from
    # subprocess.run. Pin the shell-convention normalization (128 + signal)
    # so a downstream `sys.exit(code)` can't land on the wrong exit status
    # via Python's exit-code modulo (sys.exit(-9) -> 247, not -9).
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): -9})
    monkeypatch.setattr(subprocess, "run", fake)
    fmt_gate = next(gate for gate in manifest.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(manifest, "GATES", (fmt_gate,))
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    assert rc == 137
    out = capsys.readouterr().out
    assert out == "[git-hook] rust_fmt_check: run\n[git-hook] rust_fmt_check: fail (137)\n"


def test_execute_gates_missing_gate_binary_prints_fail_line_then_raises(monkeypatch, capsys):
    # A gate binary absent from PATH (e.g. no `cargo` installed) makes
    # subprocess.run raise FileNotFoundError before any CompletedProcess
    # exists. This must not silently traceback with no fail(...) line --
    # the one-line-per-gate contract still applies, then the exception
    # propagates (fail-loud: a broken environment, not a recoverable gate
    # failure).
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): FileNotFoundError(
                2, "No such file or directory", "cargo"
            )
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    fmt_gate = next(gate for gate in manifest.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(manifest, "GATES", (fmt_gate,))
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    with pytest.raises(FileNotFoundError):
        execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "rust_fmt_check: fail" in out


# --- execute_gates -- unknown=True prints the escalation reason -----------


def test_execute_gates_prints_escalation_reason_when_unknown(monkeypatch, capsys):
    fake = _FakeGateRun({("python3", "scripts/test_run_git_hook.py"): 0})
    monkeypatch.setattr(subprocess, "run", fake)
    hook_gate = next(gate for gate in manifest.GATES if gate.name == "hook_self_test")
    monkeypatch.setattr(manifest, "GATES", (hook_gate,))
    changes = manifest.ChangeSet(paths=(), unknown=True, reason="git diff failed mid-batch")
    execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "git diff failed mid-batch" in out


def test_execute_gates_no_escalation_line_when_known(monkeypatch, capsys):
    fake = _FakeGateRun({})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "escalat" not in out.lower()


def test_execute_gates_escalation_always_explains_itself(monkeypatch, capsys):
    # CONTRACT CHANGE. This test previously pinned the `unknown=True,
    # reason=None` combination: escalate to every phase-matching gate while
    # printing no escalation line at all. That state is now unconstructible
    # (ChangeSet.__post_init__ rejects it -- see
    # test_changeset_rejects_escalation_without_a_reason), because a silent
    # full run contradicts ChangeSet's own docstring and leaves a surprising
    # run looking arbitrary. What is pinned here now is the replacement
    # guarantee: whenever escalation happens, the reason is printed AND every
    # phase-matching gate still runs.
    fake = _FakeGateRun({("python3", "scripts/test_run_git_hook.py"): 0})
    monkeypatch.setattr(subprocess, "run", fake)
    hook_gate = next(gate for gate in manifest.GATES if gate.name == "hook_self_test")
    monkeypatch.setattr(manifest, "GATES", (hook_gate,))
    changes = manifest.ChangeSet(paths=(), unknown=True, reason="git diff exited 128")
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert "escalating: git diff exited 128" in out
    assert rc == 0
    assert [cmd for cmd, _kwargs in fake.calls] == [
        ["python3", "scripts/test_run_git_hook.py"]
    ]


# --- execute_gates -- invalid phase still fails loudly (delegates to
# resolve_gates's own guard, not re-implemented) ----------------------------


def test_execute_gates_unknown_phase_raises():
    changes = manifest.ChangeSet(paths=(), unknown=False, reason=None)
    with pytest.raises(ValueError):
        execute.execute_gates("typo-phase", changes)


# --- Task 9 Part C: a completed run states plainly that it completed ------
#
# `[pre-commit] staged files: N` and "no local gates required" were removed
# with no replacement when the manifest resolver replaced the old
# run_pre_commit()/run_pre_push() conditional assembly: a docs/inert-only
# commit printed only `skip (...)` lines, with nothing stating the run
# completed intentionally rather than, say, crashing silently before
# printing anything. This restores an equivalent completion statement.


def test_execute_gates_prints_no_local_gates_required_when_nothing_selected(monkeypatch, capsys):
    fake = _FakeGateRun({})
    monkeypatch.setattr(subprocess, "run", fake)
    # Real GATES manifest, all-docs change: every gate is phase-matching but
    # none is selected (docs is inert), so every gate prints a skip line --
    # the plain completion statement must still appear after them.
    changes = manifest.ChangeSet(paths=("README.md",), unknown=False, reason=None)
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert rc == 0
    assert out.splitlines()[-1] == "[git-hook] pre-commit: no local gates required"


def test_execute_gates_prints_no_local_gates_required_for_inert_config_only(monkeypatch, capsys):
    fake = _FakeGateRun({})
    monkeypatch.setattr(subprocess, "run", fake)
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/schema/example.json",), unknown=False, reason=None
    )
    rc = execute.execute_gates(manifest.PHASE_PRE_PUSH, changes)
    out = capsys.readouterr().out
    assert rc == 0
    assert out.splitlines()[-1] == "[git-hook] pre-push: no local gates required"


def test_execute_gates_prints_completion_summary_when_gates_run(monkeypatch, capsys):
    fake = _FakeGateRun(
        {
            ("cargo", "fmt", "--all", "--", "--check"): 0,
            (
                "cargo",
                "clippy",
                "--workspace",
                "--all-targets",
                "--all-features",
                "--",
                "-D",
                "warnings",
            ): 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, _only_rust_changes())
    out = capsys.readouterr().out
    assert rc == 0
    assert out.splitlines()[-1] == "[git-hook] pre-commit: 2 gate(s) run, 0 failed"


def test_execute_gates_no_completion_line_when_a_gate_fails(monkeypatch, capsys):
    # A failed gate returns early -- the run did not complete, so no
    # completion line (of either flavor) should print.
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): 3})
    monkeypatch.setattr(subprocess, "run", fake)
    fmt_gate = next(gate for gate in manifest.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(manifest, "GATES", (fmt_gate,))
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    rc = execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    out = capsys.readouterr().out
    assert rc == 3
    assert "no local gates required" not in out
    assert "gate(s) run" not in out


# --- _scrub_git_env -- the security-critical part of this task ------------
#
# This is the direct regression guard for PR #186: a pre-push hook exporting
# GIT_DIR/GIT_INDEX_FILE/GIT_WORK_TREE, inherited by a `cargo test` tempdir
# Git fixture, committed junk onto the real branch. These tests assert on
# the env dict itself, not a call count.


def test_scrub_git_env_strips_repo_redirecting_vars():
    source = {
        "GIT_DIR": "/tmp/evil/.git",
        "GIT_INDEX_FILE": "/tmp/evil/index",
        "GIT_WORK_TREE": "/tmp/evil",
        "GIT_OBJECT_DIRECTORY": "/tmp/evil/objects",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES": "/tmp/evil/alt",
        "GIT_COMMON_DIR": "/tmp/evil/common",
        "GIT_NAMESPACE": "evil-namespace",
        "PATH": "/usr/bin",
    }
    scrubbed = runtime._scrub_git_env(source)
    for dangerous_key in (
        "GIT_DIR",
        "GIT_INDEX_FILE",
        "GIT_WORK_TREE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_COMMON_DIR",
        "GIT_NAMESPACE",
    ):
        assert dangerous_key not in scrubbed
    assert scrubbed["PATH"] == "/usr/bin"


def test_scrub_git_env_keeps_explicit_keep_list():
    # NOTE: this test previously also asserted GIT_CONFIG_COUNT/
    # GIT_CONFIG_KEY_0/GIT_CONFIG_VALUE_0 were kept. That was a genuine spec
    # bug, not a preference: GIT_CONFIG_* is the documented equivalent of
    # `git -c <key>=<value>` for arbitrary config, including core.worktree --
    # the config equivalent of GIT_WORK_TREE, which this same module strips
    # as a repo-redirecting variable a few lines above. Keeping GIT_CONFIG_*
    # let GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.worktree
    # GIT_CONFIG_VALUE_0=/real/repo reproduce the exact PR #186 redirection
    # this scrub exists to prevent. The contract changed to strip
    # GIT_CONFIG_*; see test_scrub_git_env_strips_git_config_star below for
    # the corrected behavior.
    source = {
        "GIT_ASKPASS": "/usr/bin/askpass",
        "GIT_SSH": "/usr/bin/ssh",
        "GIT_SSH_COMMAND": "ssh -i key",
        "GIT_TERMINAL_PROMPT": "0",
        "GIT_TRACE": "1",
        "GIT_TRACE_PACKET": "1",
    }
    scrubbed = runtime._scrub_git_env(source)
    assert scrubbed == source


def test_scrub_git_env_strips_git_config_star():
    # Regression guard for the GIT_CONFIG_* keep-list bug: GIT_CONFIG_COUNT +
    # GIT_CONFIG_KEY_n/GIT_CONFIG_VALUE_n are the documented equivalent of
    # `git -c <key>=<value>` -- arbitrary config, including core.worktree
    # (the config equivalent of GIT_WORK_TREE). GIT_CONFIG_GLOBAL/
    # GIT_CONFIG_SYSTEM (also GIT_CONFIG_-prefixed) replace whole config
    # files. All must be stripped like any other unrecognized GIT_* var.
    source = {
        "GIT_CONFIG_COUNT": "1",
        "GIT_CONFIG_KEY_0": "core.worktree",
        "GIT_CONFIG_VALUE_0": "/real/repo",
        "GIT_CONFIG_GLOBAL": "/tmp/evil/gitconfig",
        "GIT_CONFIG_SYSTEM": "/tmp/evil/gitconfig",
        "PATH": "/usr/bin",
    }
    scrubbed = runtime._scrub_git_env(source)
    for dangerous_key in (
        "GIT_CONFIG_COUNT",
        "GIT_CONFIG_KEY_0",
        "GIT_CONFIG_VALUE_0",
        "GIT_CONFIG_GLOBAL",
        "GIT_CONFIG_SYSTEM",
    ):
        assert dangerous_key not in scrubbed
    assert scrubbed["PATH"] == "/usr/bin"


def test_scrub_git_env_strips_unlisted_git_var_not_yet_enumerated():
    # Default-toward-scrubbing: an unrecognized GIT_* variable not in the
    # keep-list must be dropped, not silently passed through.
    source = {"GIT_SOME_FUTURE_FLAG": "danger", "PATH": "/usr/bin"}
    scrubbed = runtime._scrub_git_env(source)
    assert "GIT_SOME_FUTURE_FLAG" not in scrubbed
    assert scrubbed["PATH"] == "/usr/bin"


def test_scrub_git_env_passes_ambient_config_vars_through_by_design():
    # The DOCUMENTED BOUNDARY of this scrub, asserted rather than left as a
    # comment someone can quietly delete.
    #
    # HOME and XDG_CONFIG_HOME reach the same arbitrary-Git-config primitive
    # that GIT_CONFIG_* does: Git reads $XDG_CONFIG_HOME/git/config and
    # $HOME/.gitconfig, either of which can set core.worktree, core.hooksPath,
    # or credential.helper. They are nonetheless passed through, and this
    # scrub does NOT claim to stop them, for two reasons:
    #
    #   1. Provenance. This scrub exists to stop variables *Git itself injects
    #      into the hook environment* (GIT_DIR, GIT_INDEX_FILE, ...) from
    #      leaking into children -- the PR #186 bug, where a `cargo test`
    #      tempdir fixture inherited them and committed into the real repo.
    #      HOME/XDG_CONFIG_HOME are ambient developer environment, not
    #      hook-injected, and every tool the gates run already trusts them.
    #   2. PATH dominates. PATH cannot be scrubbed -- the gates need it to
    #      find git and cargo -- and whoever can set PATH can substitute the
    #      git binary outright, which is strictly more power than redirecting
    #      its config. Stripping XDG_CONFIG_HOME while PATH stays writable
    #      would be theater, not defense.
    #
    # So the honest invariant is "no Git-injected variable leaks into a child",
    # NOT "no kept variable can influence Git". Pinned here in that form.
    source = {
        "HOME": "/tmp/evil",
        "XDG_CONFIG_HOME": "/tmp/evil/config",
        "PATH": "/tmp/evil/bin",
    }
    assert runtime._scrub_git_env(source) == source


def test_scrub_git_env_strips_every_var_git_injects_into_a_hook():
    # The invariant the scrub DOES guarantee, stated as the closed set it
    # actually covers: the variables Git sets in a hook's environment.
    git_injected = {
        "GIT_DIR": "/real/repo/.git",
        "GIT_WORK_TREE": "/real/repo",
        "GIT_INDEX_FILE": "/real/repo/.git/index",
        "GIT_COMMON_DIR": "/real/repo/.git",
        "GIT_OBJECT_DIRECTORY": "/real/repo/.git/objects",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES": "/other/objects",
        "GIT_NAMESPACE": "refs/namespaces/evil",
        "GIT_PREFIX": "subdir/",
        "GIT_EXEC_PATH": "/tmp/evil/git-core",
    }
    scrubbed = runtime._scrub_git_env({**git_injected, "PATH": "/usr/bin"})
    leaked = sorted(key for key in git_injected if key in scrubbed)
    assert not leaked, f"Git-injected variable(s) {leaked} leaked into the child env"
    assert scrubbed["PATH"] == "/usr/bin"


def test_scrub_git_env_returns_new_dict_never_mutates_source():
    source = {"GIT_DIR": "/tmp/evil", "PATH": "/usr/bin"}
    original = dict(source)
    runtime._scrub_git_env(source)
    assert source == original


# --- execute_gates -- env scrub wired end-to-end into the subprocess call -


def test_execute_gates_scrubs_git_env_before_running_a_gate(monkeypatch):
    monkeypatch.setenv("GIT_DIR", "/tmp/evil/.git")
    monkeypatch.setenv("GIT_INDEX_FILE", "/tmp/evil/index")
    monkeypatch.setenv("GIT_WORK_TREE", "/tmp/evil")
    monkeypatch.setenv("GIT_ASKPASS", "/usr/bin/askpass")
    fake = _FakeGateRun({("cargo", "fmt", "--all", "--", "--check"): 0})
    monkeypatch.setattr(subprocess, "run", fake)
    fmt_gate = next(gate for gate in manifest.GATES if gate.name == "rust_fmt_check")
    monkeypatch.setattr(manifest, "GATES", (fmt_gate,))
    changes = manifest.ChangeSet(
        paths=("crates/ironmem/src/hook.rs",), unknown=False, reason=None
    )
    execute.execute_gates(manifest.PHASE_PRE_COMMIT, changes)
    _cmd, kwargs = fake.calls[0]
    child_env = kwargs["env"]
    assert "GIT_DIR" not in child_env
    assert "GIT_INDEX_FILE" not in child_env
    assert "GIT_WORK_TREE" not in child_env
    assert child_env.get("GIT_ASKPASS") == "/usr/bin/askpass"
    # Non-Git variables inherited from the real environment must survive.
    assert child_env.get("PATH") == os.environ.get("PATH")


# --- Task 6: main(phase) -- collect -> resolve -> execute, wired end-to-end
#
# No test in this section invokes real Git or a real gate command. Every
# `subprocess.run` call goes through `_FakeSubprocessRun` below: unlike
# `_FakeGitRun` (asserts `cmd[0] == "git"`) or `_FakeGateRun` (keyed only on
# gate argv), `main()` drives both kinds of call through the same
# `subprocess.run` seam in one test, so this fake is keyed on the exact full
# argv tuple regardless of program name. An unanticipated call raises
# KeyError, proving each test drives an exact, intentional call sequence.


class _FakeSubprocessRun:
    """Stand-in for `subprocess.run` used by every Task 6 `main()`/
    `_cli_main()` test. `responses` maps a full argv tuple to either
    `(returncode, stdout)` (the shape Git collection calls need), a bare
    `int` returncode (the shape gate calls need -- `execute_gates` never
    reads `stdout`), or an exception instance to raise.
    """

    def __init__(self, responses):
        self.responses = responses
        self.calls: list[list[str]] = []

    def __call__(self, cmd, **kwargs):
        self.calls.append(list(cmd))
        outcome = self.responses[tuple(cmd)]
        if isinstance(outcome, BaseException):
            raise outcome
        if isinstance(outcome, tuple):
            returncode, stdout = outcome
            return subprocess.CompletedProcess(cmd, returncode, stdout=stdout, stderr="")
        return subprocess.CompletedProcess(cmd, outcome)


_RUST_FMT_ARGV = ("cargo", "fmt", "--all", "--", "--check")
_RUST_CLIPPY_ARGV = (
    "cargo",
    "clippy",
    "--workspace",
    "--all-targets",
    "--all-features",
    "--",
    "-D",
    "warnings",
)
_RUST_TEST_ARGV = ("cargo", "test", "--workspace")
_HOOK_SELF_TEST_ARGV = ("python3", "scripts/test_run_git_hook.py")
_HOOK_INSTALL_CHECK_ARGV = ("bash", "scripts/install-git-hooks.sh", "--check")
_COLLAB_LINT_ARGV = ("python3", "scripts/check_collab_turn_templates.py")


# --- main() -- end-to-end, one phase at a time: a Rust-only change selects
# exactly today's gate set for that phase (exact sequence, not a count) ----


def test_main_pre_commit_rust_only_change_runs_exact_gate_sequence(monkeypatch):
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--cached", "--name-only", "--no-renames", "-z"): (
                0,
                "crates/ironmem/src/hook.rs\0",
            ),
            _RUST_FMT_ARGV: 0,
            _RUST_CLIPPY_ARGV: 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    rc = cli.main(manifest.PHASE_PRE_COMMIT)
    assert rc == 0
    assert fake.calls == [
        ["git", "diff", "--cached", "--name-only", "--no-renames", "-z"],
        list(_RUST_FMT_ARGV),
        list(_RUST_CLIPPY_ARGV),
    ]


def test_main_pre_push_rust_only_change_runs_exact_gate_sequence(monkeypatch):
    stdin = _pre_push_line("refs/heads/feature", SHA_B, "refs/heads/feature", SHA_A) + "\n"
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--name-only", "--no-renames", "-z", f"{SHA_A}..{SHA_B}"): (
                0,
                "crates/ironmem/src/hook.rs\0",
            ),
            _RUST_TEST_ARGV: 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(stdin))
    rc = cli.main(manifest.PHASE_PRE_PUSH)
    assert rc == 0
    assert fake.calls == [
        ["git", "diff", "--name-only", "--no-renames", "-z", f"{SHA_A}..{SHA_B}"],
        list(_RUST_TEST_ARGV),
    ]


# --- main() -- fail-closed property, now via collect -> execute_gates
# directly (no raising legacy adapter in between)
#
# Global constraint: "A Git failure must not let the hook exit 0 with zero
# gates run." Under this architecture that guarantee is upheld differently
# than the pre-Task-6 staged_paths()/pushed_paths() SystemExit: a Git
# failure sets ChangeSet.unknown=True, and resolve_gates (already wired,
# unchanged by this task) escalates unknown=True to select every
# phase-matching gate -- main() never special-cases this, it falls out
# directly from collect -> execute_gates. So the property proven here is
# "every phase gate is actually attempted, and its real result decides the
# exit code" -- never "silently present as success having run nothing".


def test_main_pre_commit_git_failure_escalates_to_every_gate_never_zero_gates_run(
    monkeypatch, capsys
):
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--cached", "--name-only", "--no-renames", "-z"): (128, ""),
            _HOOK_SELF_TEST_ARGV: 0,
            _HOOK_INSTALL_CHECK_ARGV: 0,
            _COLLAB_LINT_ARGV: 0,
            _RUST_FMT_ARGV: 0,
            _RUST_CLIPPY_ARGV: 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    rc = cli.main(manifest.PHASE_PRE_COMMIT)
    out = capsys.readouterr().out
    assert rc == 0
    assert "escalating" in out
    non_git_calls = [cmd for cmd in fake.calls if cmd[0] != "git"]
    # Every pre-commit gate was actually attempted -- the old defect ("no
    # staged files; skipping gates", exit 0, zero gates run) cannot recur: a
    # Git failure here forces the full phase gate set to run.
    assert non_git_calls == [
        list(_HOOK_SELF_TEST_ARGV),
        list(_HOOK_INSTALL_CHECK_ARGV),
        list(_COLLAB_LINT_ARGV),
        list(_RUST_FMT_ARGV),
        list(_RUST_CLIPPY_ARGV),
    ]


def test_main_pre_commit_git_failure_escalated_gate_failure_is_nonzero_exit(monkeypatch):
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--cached", "--name-only", "--no-renames", "-z"): (128, ""),
            _HOOK_SELF_TEST_ARGV: 1,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    rc = cli.main(manifest.PHASE_PRE_COMMIT)
    assert rc == 1


def test_main_pre_push_git_failure_escalates_to_every_gate(monkeypatch, capsys):
    stdin = _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A) + "\n"
    fake = _FakeSubprocessRun(
        {
            ("git", "diff", "--name-only", "--no-renames", "-z", f"{SHA_A}..{SHA_B}"): (128, ""),
            _HOOK_SELF_TEST_ARGV: 0,
            _COLLAB_LINT_ARGV: 0,
            _RUST_TEST_ARGV: 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(stdin))
    rc = cli.main(manifest.PHASE_PRE_PUSH)
    out = capsys.readouterr().out
    assert rc == 0
    assert "escalating" in out
    non_git_calls = [cmd for cmd in fake.calls if cmd[0] != "git"]
    assert non_git_calls == [
        list(_HOOK_SELF_TEST_ARGV),
        list(_COLLAB_LINT_ARGV),
        list(_RUST_TEST_ARGV),
    ]
    # The @{u} fallback must never fire once unknown=True -- escalation, not
    # the manual-invocation fallback, is the fail-closed path here.
    assert ("git", "rev-parse", "--verify", "@{u}") not in [tuple(c) for c in fake.calls]


def test_main_unknown_phase_raises_before_any_io(monkeypatch):
    def poison(cmd, **kwargs):
        raise AssertionError(f"unexpected subprocess.run call: {cmd}")

    monkeypatch.setattr(subprocess, "run", poison)
    with pytest.raises(ValueError):
        cli.main("typo-phase")


# --- main("pre-push") -- @{u} fallback for manual/direct invocation -------
#
# DECISION (see task-6-report.md for the full rationale): kept, ported from
# the retired pushed_paths()'s `@{u}` fallback. The real `git push`-invoked
# hook always pipes ref-update lines to stdin, so collect_pre_push_changes
# never legitimately sees empty/no-line stdin in that path; this fallback
# exists solely for a developer running
# `python3 scripts/run_git_hook.py pre-push` directly with no piped stdin.
# Implemented in main() itself, not inside collect_pre_push_changes() --
# that function's empty-stdin contract (unknown=False, paths=(), no Git
# calls -- see test_collect_pre_push_changes_empty_stdin_is_not_unknown) is
# Task 4's already-tested collection-layer behavior and is not touched here.


def test_main_pre_push_manual_invocation_falls_back_to_upstream(monkeypatch):
    fake = _FakeSubprocessRun(
        {
            ("git", "rev-parse", "--verify", "@{u}"): (0, SHA_A + "\n"),
            ("git", "diff", "--name-only", "--no-renames", "-z", f"{SHA_A}..HEAD"): (
                0,
                "crates/ironmem/src/hook.rs\0",
            ),
            _RUST_TEST_ARGV: 0,
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(""))
    rc = cli.main(manifest.PHASE_PRE_PUSH)
    assert rc == 0
    assert fake.calls == [
        ["git", "rev-parse", "--verify", "@{u}"],
        ["git", "diff", "--name-only", "--no-renames", "-z", f"{SHA_A}..HEAD"],
        list(_RUST_TEST_ARGV),
    ]


def test_main_pre_push_whitespace_only_stdin_escalates_and_never_reaches_fallback(
    monkeypatch, capsys
):
    # Pins the corrected docstring claim: main()/_pre_push_manual_upstream_
    # changes() used to say the @{u} fallback fires when stdin is "empty or
    # whitespace-only". Whitespace-only stdin is a malformed ref-update line,
    # so collect_pre_push_changes returns unknown=True and the `not
    # changes.unknown` guard keeps the fallback unreachable -- fail-closed,
    # which is the correct behavior and is deliberately left unchanged. Only
    # genuinely empty stdin reaches the fallback.
    fake = _FakeSubprocessRun(
        {_HOOK_SELF_TEST_ARGV: 0, _COLLAB_LINT_ARGV: 0, _RUST_TEST_ARGV: 0}
    )
    monkeypatch.setattr(subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub("   \n"))
    rc = cli.main(manifest.PHASE_PRE_PUSH)
    out = capsys.readouterr().out
    assert rc == 0
    assert "[git-hook] escalating: malformed pre-push stdin line 1" in out
    # Escalation means EVERY pre-push gate ran -- assert the actual call list,
    # not just the absence of git calls. The previous "no git call happened"
    # check was satisfied by a run that escalated and then ran nothing at all,
    # which is the fail-open outcome this test exists to rule out.
    assert fake.calls == [
        list(_HOOK_SELF_TEST_ARGV),
        list(_COLLAB_LINT_ARGV),
        list(_RUST_TEST_ARGV),
    ]


def test_main_pre_push_manual_invocation_no_upstream_runs_no_gates(monkeypatch):
    fake = _FakeSubprocessRun({("git", "rev-parse", "--verify", "@{u}"): (128, "")})
    monkeypatch.setattr(subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(""))
    rc = cli.main(manifest.PHASE_PRE_PUSH)
    assert rc == 0
    assert fake.calls == [["git", "rev-parse", "--verify", "@{u}"]]


def test_main_pre_push_manual_invocation_upstream_diff_failure_raises_systemexit(monkeypatch):
    # Ported behavior: the fallback's diff call uses the legacy `git()`
    # helper at its default `check=True`, so a Git failure here aborts
    # immediately via SystemExit -- zero gates run, matching the literal
    # pre-Task-6 pushed_paths() `@{u}` fallback behavior (distinct from the
    # primary collection path above, which escalates instead of aborting).
    fake = _FakeSubprocessRun(
        {
            ("git", "rev-parse", "--verify", "@{u}"): (0, SHA_A + "\n"),
            ("git", "diff", "--name-only", "--no-renames", "-z", f"{SHA_A}..HEAD"): (128, ""),
        }
    )
    monkeypatch.setattr(subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(""))
    with pytest.raises(SystemExit):
        cli.main(manifest.PHASE_PRE_PUSH)


def test_main_pre_push_with_stdin_paths_never_triggers_upstream_fallback(monkeypatch):
    stdin = _pre_push_line("refs/heads/a", SHA_B, "refs/heads/a", SHA_A) + "\n"
    fake = _FakeSubprocessRun(
        {("git", *_range_diff(SHA_A, SHA_B)): (0, "README.md\0")}
    )
    monkeypatch.setattr(subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(stdin))
    rc = cli.main(manifest.PHASE_PRE_PUSH)
    assert rc == 0
    # Non-empty paths (even docs-only, which selects zero gates) must never
    # trigger the @{u} fallback -- it is reserved for the genuinely-empty
    # case only.
    assert fake.calls == [["git", "diff", "--name-only", "--no-renames", "-z", f"{SHA_A}..{SHA_B}"]]


def test_main_pre_push_deletion_only_push_does_not_trigger_upstream_fallback(monkeypatch):
    # Regression test for the false "unreachable from a real git push" claim
    # (scripts/run_git_hook.py's _pre_push_manual_upstream_changes docstring
    # and docs/CODEX.md both claimed this). `git push --delete branch` pipes
    # a real, non-empty stdin line whose local_sha is ZERO_SHA -- every line
    # hits collect_pre_push_changes's deletion-ref `continue`, so the
    # resulting ChangeSet is genuinely paths=(), unknown=False despite stdin
    # being non-empty. The old `not changes.paths` gate in main() could not
    # tell that apart from a manual invocation with no piped stdin at all,
    # and would fire the @{u} fallback -- diffing the *checked-out* branch's
    # upstream range, unrelated to the deletion being pushed. Gating on
    # `stdin_text.strip()` instead means a real deletion-only push (non-empty
    # stdin) never reaches the fallback: no git calls at all, since there is
    # nothing to diff for a pure ref deletion.
    stdin = _pre_push_line("refs/heads/gone", ZERO_SHA_1, "refs/heads/gone", SHA_A) + "\n"
    fake = _FakeSubprocessRun({})  # no git call of any kind is expected
    monkeypatch.setattr(subprocess, "run", fake)
    monkeypatch.setattr(sys, "stdin", _StdinStub(stdin))
    rc = cli.main(manifest.PHASE_PRE_PUSH)
    assert rc == 0
    assert fake.calls == []


# --- _cli_main(argv) -- the usage-error / exit-2 contract, preserved from
# the pre-Task-6 main(argv) ---------------------------------------------
#
# No subprocess mocking needed for the invalid-argv cases: validation must
# reject before any collection or gate I/O is attempted.


def test_cli_main_missing_argument_prints_usage_and_returns_2(monkeypatch, capsys):
    # `_cli_main` must reject a missing argument and return before ever
    # calling `main()` -- poison `subprocess.run` so that if this guard ever
    # regressed (e.g. started calling `main()` on invalid argv), the test
    # fails loudly on an unexpected subprocess call instead of silently
    # invoking real Git and real gates.
    def poison(cmd, **kwargs):
        raise AssertionError(f"unexpected subprocess.run call: {cmd}")

    monkeypatch.setattr(subprocess, "run", poison)
    assert cli._cli_main(["scripts/run_git_hook.py"]) == 2
    err = capsys.readouterr().err
    assert err == "usage: scripts/run_git_hook.py <pre-commit|pre-push>\n"


def test_cli_main_bad_argument_prints_usage_and_returns_2(capsys):
    assert cli._cli_main(["scripts/run_git_hook.py", "typo-phase"]) == 2
    err = capsys.readouterr().err
    assert err == "usage: scripts/run_git_hook.py <pre-commit|pre-push>\n"


def test_cli_main_extra_arguments_prints_usage_and_returns_2(capsys):
    assert cli._cli_main(["scripts/run_git_hook.py", "pre-commit", "extra"]) == 2


def test_cli_main_valid_phase_delegates_to_main(monkeypatch):
    calls = []

    def fake_main(phase):
        calls.append(phase)
        return 0

    monkeypatch.setattr(cli, "main", fake_main)
    assert cli._cli_main(["scripts/run_git_hook.py", "pre-commit"]) == 0
    assert calls == ["pre-commit"]


def test_cli_main_propagates_main_exit_code(monkeypatch):
    monkeypatch.setattr(cli, "main", lambda phase: 3)
    assert cli._cli_main(["scripts/run_git_hook.py", "pre-push"]) == 3


# --- module-wide static guard -----------------------------------------------


def test_script_entry_point_is_wired_as_a_subprocess():
    # Every other test calls main()/_cli_main() in-process, so a
    # `run_git_hook.py` missing its `if __name__ == "__main__"` block -- or
    # unable to import its own package because sys.path[0] isn't scripts/ --
    # passes all of them while the real hook silently does nothing and exits
    # 0. That is a total fail-open: `git commit` would run no gates at all.
    # Caught in exactly that way when this module was split into a package.
    #
    # The invalid-phase path is used deliberately: it exercises __main__ ->
    # _cli_main end-to-end but returns before any gate runs, so this test
    # cannot recursively invoke cargo (it runs *inside* the hook self-test
    # gate, which the pre-commit hook itself calls).
    result = subprocess.run(
        [sys.executable, str(SCRIPTS / "run_git_hook.py"), "not-a-phase"],
        capture_output=True,
        text=True,
        cwd=ROOT,
        check=False,
    )
    assert result.returncode == 2, (
        f"expected usage exit 2, got {result.returncode}; "
        f"stdout={result.stdout!r} stderr={result.stderr!r}"
    )
    assert "usage: scripts/run_git_hook.py <pre-commit|pre-push>" in result.stderr


def _hook_source_files() -> list[pathlib.Path]:
    """Every Python file implementing the hook, discovered rather than listed.

    A hardcoded list would have silently stopped covering the extracted
    modules the moment run_git_hook.py was split -- which is exactly when the
    static guards below matter most.
    """
    return sorted([SCRIPTS / "run_git_hook.py", *(SCRIPTS / "git_hook").glob("*.py")])


def test_module_source_never_uses_shell_true():
    offenders = [
        path.name for path in _hook_source_files() if "shell=True" in path.read_text()
    ]
    assert not offenders, f"shell=True found in {offenders}"


def test_hook_exact_paths_covers_every_git_hook_module():
    # HOOK_EXACT_PATHS is what makes an edit to the hook select the hook
    # self-test gate. A module added to scripts/git_hook/ without an entry
    # there would classify inert_config (a .py under no declared surface is
    # UNKNOWN, but the package is not special-cased anywhere else) and could
    # change gate resolution without ever running the suite that validates
    # gate resolution. Walk the directory instead of trusting the list.
    on_disk = {
        path.relative_to(ROOT).as_posix() for path in (SCRIPTS / "git_hook").glob("*.py")
    }
    missing = sorted(on_disk - manifest.HOOK_EXACT_PATHS)
    assert not missing, (
        f"git_hook module(s) {missing} are not in HOOK_EXACT_PATHS, so editing them "
        "would not select the hook self-test gate"
    )
    # And the reverse: no stale entry for a module that no longer exists.
    declared = {
        path for path in manifest.HOOK_EXACT_PATHS if path.startswith("scripts/git_hook/")
    }
    stale = sorted(declared - on_disk)
    assert not stale, f"HOOK_EXACT_PATHS names non-existent module(s) {stale}"


def test_every_git_hook_module_classifies_hook_self_test():
    # The end-to-end property the entry above only half-guarantees: each hook
    # module must actually resolve to SURFACE_HOOK_SELF_TEST.
    for path in (SCRIPTS / "git_hook").glob("*.py"):
        relative = path.relative_to(ROOT).as_posix()
        assert manifest.classify_path(relative) == manifest.SURFACE_HOOK_SELF_TEST, (
            f"{relative} must classify hook_self_test so editing it runs the "
            "hook's own test suite"
        )


# --- __main__ delegation must fail loudly, never exit 0, if pytest is absent ---


def test_missing_pytest_fails_loudly_with_an_actionable_message(tmp_path):
    # REPLACES a monkeypatch-based test that set this module's `pytest`
    # attribute to None and called `_run_as_script()`. That asserted against a
    # branch the interpreter could never reach: by the time any function runs,
    # the module-level `@pytest.mark.parametrize` decorators have already been
    # evaluated, so a genuinely missing pytest crashed at import instead. The
    # old test passed for years of CI runs that were failing on a traceback.
    #
    # This drives the real path: a subprocess whose sys.path leads with a
    # `pytest.py` that raises ImportError, which is what an uninstalled pytest
    # actually looks like to the import system.
    (tmp_path / "pytest.py").write_text('raise ImportError("pytest not installed")\n')
    env = {**os.environ, "PYTHONPATH": str(tmp_path)}
    result = subprocess.run(
        [sys.executable, str(SCRIPTS / "test_run_git_hook.py")],
        capture_output=True,
        text=True,
        cwd=ROOT,
        env=env,
        check=False,
    )
    assert result.returncode == 1, (
        f"expected a clean exit 1, got {result.returncode}; "
        f"stderr={result.stderr!r}"
    )
    assert "pip install pytest" in result.stderr
    # The specific regression: a traceback here means the friendly message is
    # unreachable again.
    assert "Traceback" not in result.stderr, result.stderr


def _run_as_script() -> int:
    return pytest.main([__file__])


if __name__ == "__main__":
    raise SystemExit(_run_as_script())
