//! Onboarder — rung 3 of the build ladder (spec's *Repo onboarding* section,
//! step 2): "A one-shot Onboarder agent inspects the repo (CI config,
//! `Cargo.toml`/`package.json`/`Makefile`) and writes a **proposed** gate
//! config drawer in `pending` state."
//!
//! This module owns the *inference* half only — [`gate_config`] already
//! implements the `pending` → `approved` storage/state machine (rung 1);
//! [`onboard_repo`] is the glue that calls [`infer_gate_commands`] and hands
//! the result to [`gate_config::propose_gate_config`].
//!
//! # Scope: manifest files, not CI config
//!
//! The spec's phrase "CI config, `Cargo.toml`/`package.json`/`Makefile`"
//! names CI config first, but a real `.github/workflows/*.yml` (or other CI
//! provider) can express arbitrary matrix builds, multi-step jobs, and
//! shell logic — parsing that reliably enough to *trust its output as an
//! unattended gate* is a much larder problem than this rung's testing bar
//! ("Gate inference against fixture repos (Rust, Python, Swift)"). Inference
//! here is deliberately limited to deterministic, root-level build-manifest
//! detection — the same signal a human skimming the repo root would use.
//! Nothing here recurses into subdirectories: a monorepo's vendored or
//! example subtrees must not silently contribute a gate command a human
//! onboarding the *repo* never intended to run. A repo whose real gate can
//! only be read out of CI config still onboards — a human just supplies the
//! commands directly via [`gate_config::propose_gate_config`] instead of this
//! module's inference path; [`propose_gate_config`] never required inference
//! to be its only caller.
//!
//! # Multi-stack repos
//!
//! Every recognized stack present at the repo root contributes its command;
//! results are not mutually exclusive. A `Makefile` `test:` target is only
//! consulted as a fallback when no other stack was recognized at all — a
//! Rust repo's incidental `Makefile` (e.g. a `docs:` or `release:` target)
//! must not add a redundant, unreviewed second gate command alongside the
//! `cargo test` this module already proposed.

use std::path::Path;

use crate::db::schema::Database;
use crate::error::MemoryError;

use super::gate_config::{propose_gate_config, GateConfig};

/// Build-manifest markers that indicate a Python project. Any one is
/// sufficient; the actual gate command is the same (`pytest`) regardless of
/// which marker matched — this rung does not attempt to distinguish
/// `tox`-driven suites from bare `pytest` ones.
const PYTHON_MARKERS: &[&str] = &[
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "tox.ini",
    "Pipfile",
];

/// Infer gate commands for the repo checked out at `repo_path`, by
/// deterministic root-level build-manifest detection (see module docs for
/// scope). Returns every recognized stack's command, in a fixed order
/// (Rust, Python, Swift, Node), so the same repo always infers the same
/// list. Errors if `repo_path` is not a directory, or if nothing recognized
/// was found — an empty gate is never proposed silently (rung 2's
/// `turn_prompt::render` already panics on an empty `gate_commands`; failing
/// here, with a message a human can act on, is strictly better than that
/// panic firing downstream at dispatch time).
pub fn infer_gate_commands(repo_path: &Path) -> Result<Vec<String>, MemoryError> {
    if !repo_path.is_dir() {
        return Err(MemoryError::Validation(format!(
            "'{}' is not a directory — cannot inspect it for build manifests",
            repo_path.display()
        )));
    }

    // A read/parse error on one manifest (e.g. invalid JSON in an unrelated
    // package.json) must not veto a *different* stack this function already
    // recognized — see the module doc's "every recognized stack ... contributes
    // its command" contract. Only the first such error is kept (there is
    // never a reason to report more than one root-cause to a human fixing
    // their repo one manifest at a time); it surfaces only if no stack was
    // recognized at all, in place of the generic "nothing found" message
    // below.
    let mut commands = Vec::new();
    let mut manifest_error: Option<MemoryError> = None;

    record(infer_rust(repo_path), &mut commands, &mut manifest_error);
    if let Some(cmd) = infer_python(repo_path) {
        commands.push(cmd);
    }
    if let Some(cmd) = infer_swift(repo_path) {
        commands.push(cmd);
    }
    record(infer_node(repo_path), &mut commands, &mut manifest_error);
    // Only consult the Makefile fallback when *nothing at all* was
    // recognized, per the module doc's fallback-only scope — critically,
    // that means skipping it when a manifest error occurred too, even
    // though `commands` is also empty in that case. Without the
    // `manifest_error.is_none()` half of this guard, a Cargo.toml that
    // exists but fails to read (permission error, symlink loop, ...)
    // alongside an incidental Makefile `test:` target would silently
    // substitute `make test` for the intended `cargo test` and discard the
    // real read error below, instead of surfacing it.
    if commands.is_empty() && manifest_error.is_none() {
        record(
            infer_makefile_fallback(repo_path),
            &mut commands,
            &mut manifest_error,
        );
    }

    if commands.is_empty() {
        if let Some(err) = manifest_error {
            return Err(err);
        }
        return Err(MemoryError::Validation(format!(
            "could not infer any gate commands for '{}' — no recognized build manifest \
             (Cargo.toml, {}, Package.swift, package.json) or Makefile 'test:' target found; \
             propose a gate config manually via `propose_gate_config`",
            repo_path.display(),
            PYTHON_MARKERS.join("/"),
        )));
    }
    Ok(commands)
}

/// Fold one fallible stack detector's result into the shared `commands`/
/// `manifest_error` accumulators — the identical 3-way handling
/// [`infer_rust`], [`infer_node`], and [`infer_makefile_fallback`] each need
/// in [`infer_gate_commands`], pulled out once rather than repeated at every
/// call site. Only the first error is kept, matching the "one root cause at
/// a time" contract described there.
fn record(
    result: Result<Option<String>, MemoryError>,
    commands: &mut Vec<String>,
    manifest_error: &mut Option<MemoryError>,
) {
    match result {
        Ok(Some(cmd)) => commands.push(cmd),
        Ok(None) => {}
        Err(err) => {
            manifest_error.get_or_insert(err);
        }
    }
}

/// `Cargo.toml` present → Rust. A root `[workspace]` table means member
/// crates typically don't all build/test from the root package alone, so
/// `--workspace` is required for the gate to actually cover them; a plain
/// package gets the simpler `cargo test`.
fn infer_rust(repo_path: &Path) -> Result<Option<String>, MemoryError> {
    let manifest = repo_path.join("Cargo.toml");
    if !manifest.is_file() {
        return Ok(None);
    }
    let content = read_manifest(&manifest)?;
    Ok(Some(if is_cargo_workspace(&content) {
        "cargo test --workspace".to_string()
    } else {
        "cargo test".to_string()
    }))
}

/// Whether a `Cargo.toml`'s content declares a `[workspace]` table — either
/// directly, or via a `[workspace.*]` subtable header (TOML's implicit-
/// parent-table rule means a manifest that only ever writes e.g.
/// `[workspace.package]` still establishes the `workspace` table Cargo looks
/// for; a real `Cargo.toml` combining `[workspace]` with `[workspace.package]`
/// for shared metadata is common, and this repo's own root manifest is one).
///
/// This is a line-oriented heuristic, not a real TOML parser (this crate
/// has no TOML-parsing dependency), so it tracks whether each line falls
/// inside a `"""`-delimited multi-line string — otherwise a `description`
/// field's free text merely *containing* the line `[workspace]` would be
/// mistaken for a real header. It still cannot see a workspace declared
/// purely through top-level dotted-key syntax with no header line at all
/// (e.g. `workspace.members = [...]`) — closing that gap fully would need a
/// real TOML parser, which is more machinery than this rung's fixture-repo
/// testing bar (see the module doc) asks for.
fn is_cargo_workspace(content: &str) -> bool {
    let mut in_multiline_string = false;
    for raw_line in content.lines() {
        let has_odd_triple_quotes = raw_line.matches("\"\"\"").count() % 2 == 1;
        if in_multiline_string {
            if has_odd_triple_quotes {
                in_multiline_string = false;
            }
            continue;
        }
        if has_odd_triple_quotes {
            in_multiline_string = true;
            continue;
        }
        let header = raw_line
            .split('#')
            .next()
            .expect("str::split always yields at least one item")
            .trim();
        let is_workspace_header = header
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .map(str::trim)
            .is_some_and(|name| name == "workspace" || name.starts_with("workspace."));
        if is_workspace_header {
            return true;
        }
    }
    false
}

/// The exact `npm init` default `scripts.test` placeholder — always fails,
/// so it never counts as a real gate. An exact match (rather than a
/// substring check) so a real script that merely mentions this text, e.g. a
/// fallback branch like `"jest || echo \"Error: no test specified\" && exit
/// 1"`, is still recognized as a real gate.
const NPM_INIT_PLACEHOLDER_TEST_SCRIPT: &str = "echo \"Error: no test specified\" && exit 1";

/// Any [`PYTHON_MARKERS`] file present at the root → Python, gated by
/// `pytest`.
fn infer_python(repo_path: &Path) -> Option<String> {
    PYTHON_MARKERS
        .iter()
        .any(|marker| repo_path.join(marker).is_file())
        .then(|| "pytest".to_string())
}

/// `Package.swift` present → a Swift package, gated by `swift test`. Xcode
/// project/workspace files (`.xcodeproj`/`.xcworkspace`) are deliberately
/// not handled — running their tests requires a `-scheme` name this module
/// has no reliable way to infer, and guessing wrong would silently propose a
/// gate command that fails on every dispatch.
fn infer_swift(repo_path: &Path) -> Option<String> {
    repo_path
        .join("Package.swift")
        .is_file()
        .then(|| "swift test".to_string())
}

/// `package.json` present with a real (non-placeholder) `scripts.test`
/// entry → Node, gated by `npm test`. A missing `scripts.test`, or the
/// `npm init` default placeholder (`"echo \"Error: no test specified\" &&
/// exit 1"`, which always fails), does not count as a real gate — proposing
/// it would make the gate config permanently unsatisfiable for a project
/// that simply has no test script yet.
fn infer_node(repo_path: &Path) -> Result<Option<String>, MemoryError> {
    let manifest = repo_path.join("package.json");
    if !manifest.is_file() {
        return Ok(None);
    }
    let content = read_manifest(&manifest)?;
    let value: serde_json::Value = serde_json::from_str(&content)?;
    let test_script = value
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(|script| script.as_str())
        .map(str::trim);
    Ok(match test_script {
        Some(script) if !script.is_empty() && script != NPM_INIT_PLACEHOLDER_TEST_SCRIPT => {
            Some("npm test".to_string())
        }
        _ => None,
    })
}

/// Fallback consulted only when no other stack was recognized (see module
/// docs): a makefile with a `test` target → `make test`. Checked under each
/// name GNU Make itself recognizes, in Make's own preference order
/// (`GNUmakefile`, `makefile`, `Makefile`) — but, matching real Make, only
/// the *first name that exists* is ever read. Real `make` never falls back
/// to a lower-preference file just because the file it actually loaded
/// lacks the target being asked for; it fails outright. So once this loop
/// finds the first existing name, that file's content is authoritative —
/// whether or not it has a `test` target — and the search stops rather than
/// continuing on to check the other two names' content.
fn infer_makefile_fallback(repo_path: &Path) -> Result<Option<String>, MemoryError> {
    for name in ["GNUmakefile", "makefile", "Makefile"] {
        let makefile = repo_path.join(name);
        if !makefile.is_file() {
            continue;
        }
        let content = read_manifest(&makefile)?;
        return Ok(has_test_target(&content).then(|| "make test".to_string()));
    }
    Ok(None)
}

/// Whether a Makefile's content declares a `test` target: a line whose
/// non-recipe portion (a leading tab denotes a recipe/command line, not a
/// target header — leading spaces before a target are otherwise harmless)
/// starts with `test:` or the double-colon form `test::`, followed by
/// prerequisites or nothing. This deliberately excludes a Make
/// variable-assignment line that happens to share the same prefix — `test:=`
/// (simple/immediate assignment) or `test::=` (POSIX/GNU immediate
/// assignment) — which defines a *variable* named `test`, not a target, and
/// would otherwise make `make test` fail with "No rule to make target
/// `test'" despite this function reporting a gate.
fn has_test_target(content: &str) -> bool {
    content.lines().any(|line| {
        if line.starts_with('\t') {
            return false;
        }
        line.trim_start()
            .strip_prefix("test:")
            .is_some_and(|rest| !rest.starts_with('=') && !rest.starts_with(":="))
    })
}

/// `std::fs::read_to_string`, but with the failing path folded into the
/// error message. `MemoryError::Io`'s `#[from] std::io::Error` conversion
/// alone loses the path — `std::io::Error`'s `Display` never includes it —
/// which would otherwise leave a human debugging a bare "IO error:
/// Permission denied (os error 13)" with no indication of which manifest
/// (`Cargo.toml`, `package.json`, or a `Makefile` variant) caused it.
fn read_manifest(path: &Path) -> Result<String, MemoryError> {
    std::fs::read_to_string(path).map_err(|err| {
        MemoryError::Validation(format!("failed to read '{}': {err}", path.display()))
    })
}

/// End-to-end onboarding (spec steps 1-2): infer gate commands for the local
/// checkout at `repo_path`, then write a `pending` proposal for `repo` — the
/// storage identity (e.g. `"owner/repo"`) `gate_config` keys on, which is
/// deliberately a separate parameter from `repo_path` (the physical checkout
/// used only for inspection; the Lead's dispatches later resolve their own
/// worktrees independently, per rung 2's `run_dispatch`).
pub fn onboard_repo(
    db: &Database,
    repo: &str,
    repo_path: &Path,
) -> Result<GateConfig, MemoryError> {
    let commands = infer_gate_commands(repo_path)?;
    propose_gate_config(db, repo, commands)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn infers_plain_cargo_test_for_a_non_workspace_rust_crate() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["cargo test".to_string()]
        );
    }

    #[test]
    fn infers_workspace_cargo_test_for_a_rust_workspace() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace]\nmembers = [\"crates/*\"]\n",
        );
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["cargo test --workspace".to_string()]
        );
    }

    #[test]
    fn infers_workspace_cargo_test_when_the_header_has_a_trailing_comment_or_inner_spacing() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[ workspace ]  # members below\nmembers = [\"crates/*\"]\n",
        );
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["cargo test --workspace".to_string()]
        );
    }

    #[test]
    fn infers_workspace_cargo_test_from_a_workspace_package_subtable_with_no_bare_header() {
        // `[workspace.package]` alone (no standalone `[workspace]` line)
        // still establishes the `workspace` table per TOML's implicit-
        // parent-table rule, and Cargo genuinely treats this as a workspace
        // root — this repo's own root Cargo.toml combines `[workspace]` with
        // `[workspace.package]` for exactly this reason.
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[workspace.package]\nrust-version = \"1.91\"\n",
        );
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["cargo test --workspace".to_string()]
        );
    }

    #[test]
    fn a_workspace_bracket_line_inside_a_multiline_string_is_not_a_real_header() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Cargo.toml",
            "[package]\nname = \"x\"\ndescription = \"\"\"\n[workspace]\n\"\"\"\n",
        );
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["cargo test".to_string()]
        );
    }

    #[test]
    fn infers_pytest_from_pyproject_toml() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "pyproject.toml", "[project]\nname = \"x\"\n");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["pytest".to_string()]
        );
    }

    #[test]
    fn infers_pytest_from_bare_requirements_txt() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "requirements.txt", "flask==3.0\n");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["pytest".to_string()]
        );
    }

    #[test]
    fn infers_swift_test_from_package_swift() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "Package.swift",
            "// swift-tools-version:5.9\nimport PackageDescription\n",
        );
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["swift test".to_string()]
        );
    }

    #[test]
    fn infers_npm_test_when_a_real_test_script_is_present() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"x","scripts":{"test":"jest"}}"#,
        );
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["npm test".to_string()]
        );
    }

    #[test]
    fn npm_init_placeholder_test_script_is_not_treated_as_a_real_gate() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"x","scripts":{"test":"echo \"Error: no test specified\" && exit 1"}}"#,
        );
        // No other stack present either, so this must fail closed rather
        // than silently proposing an always-failing gate.
        assert!(infer_gate_commands(dir.path()).is_err());
    }

    #[test]
    fn missing_scripts_test_key_is_not_a_gate() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "package.json", r#"{"name":"x","scripts":{}}"#);
        assert!(infer_gate_commands(dir.path()).is_err());
    }

    #[test]
    fn a_test_script_that_merely_mentions_the_npm_placeholder_text_is_still_a_real_gate() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "package.json",
            r#"{"name":"x","scripts":{"test":"jest || echo \"Error: no test specified\" && exit 1"}}"#,
        );
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["npm test".to_string()]
        );
    }

    #[test]
    fn a_malformed_package_json_does_not_discard_an_already_recognized_rust_stack() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        write(dir.path(), "package.json", "{ not valid json");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["cargo test".to_string()]
        );
    }

    #[test]
    fn multi_stack_repo_unions_every_recognized_command_in_a_fixed_order() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        write(dir.path(), "pyproject.toml", "[project]\nname = \"x\"\n");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["cargo test".to_string(), "pytest".to_string()]
        );
    }

    #[test]
    fn makefile_test_target_is_used_only_when_nothing_else_is_recognized() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Makefile", "test:\n\t./run_tests.sh\n");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["make test".to_string()]
        );
    }

    #[test]
    fn lowercase_makefile_test_target_is_recognized_too() {
        // On a case-sensitive filesystem (as CI runs), a lowercase `makefile`
        // is a distinct file from `Makefile` and must still be found — GNU
        // Make itself honors both. (On a case-insensitive filesystem this
        // still passes, just without exercising the case-sensitive path.)
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "makefile", "test:\n\t./run_tests.sh\n");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["make test".to_string()]
        );
    }

    #[test]
    fn makefile_is_ignored_when_a_recognized_stack_is_already_present() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        write(dir.path(), "Makefile", "test:\n\t./run_tests.sh\n");
        // Only the Rust command — the Makefile's target is not additionally
        // included per the module's documented fallback-only scope.
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["cargo test".to_string()]
        );
    }

    #[test]
    fn makefile_without_a_test_target_does_not_count() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Makefile", "release:\n\t./ship.sh\n");
        assert!(infer_gate_commands(dir.path()).is_err());
    }

    #[test]
    fn gnu_makefile_precedence_wins_even_without_a_test_target_there() {
        // Real `make` loads only the first of GNUmakefile/makefile/Makefile
        // that exists and never falls through to the others. A GNUmakefile
        // with no `test:` target means `make test` really fails, even
        // though a sibling Makefile happens to have one — this must not be
        // detected as a working gate.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "GNUmakefile", "build:\n\t./build.sh\n");
        write(dir.path(), "Makefile", "test:\n\t./run_tests.sh\n");
        assert!(infer_gate_commands(dir.path()).is_err());
    }

    #[test]
    fn makefile_variable_assignment_is_not_mistaken_for_a_test_target() {
        // `test:=...` (Make's simple/immediate-assignment operator) defines
        // a variable named `test`, not a target — `make test` would fail
        // with "No rule to make target `test'" against this file.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Makefile", "test:=$(wildcard tests/*.py)\n");
        assert!(infer_gate_commands(dir.path()).is_err());
    }

    #[test]
    fn makefile_test_target_with_leading_whitespace_is_recognized() {
        // Only a leading TAB denotes a recipe line in Make; leading spaces
        // before a target header are harmless and `make test` really works
        // against this file.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Makefile", " test: build\n\t./run_tests.sh\n");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["make test".to_string()]
        );
    }

    #[test]
    fn makefile_double_colon_test_target_is_still_recognized() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Makefile", "test:: unit\n\t./run_unit.sh\n");
        assert_eq!(
            infer_gate_commands(dir.path()).unwrap(),
            vec!["make test".to_string()]
        );
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_cargo_toml_is_not_masked_by_an_incidental_makefile_test_target() {
        // Regression test for a conflation bug: the Makefile fallback must
        // only fire when *nothing* was recognized, not merely when
        // `commands` is empty — those are different when Cargo.toml exists
        // but fails to read. Without this guard, the real Cargo.toml error
        // was silently discarded in favor of `make test`.
        //
        // Uses a real unreadable file via Unix permission bits, so this is
        // skipped in effect (though not compiled out) when the test runner
        // executes as root, which ignores permission bits entirely.
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[package]\nname = \"x\"\n");
        write(dir.path(), "Makefile", "test:\n\t./run_tests.sh\n");
        let cargo_toml = dir.path().join("Cargo.toml");
        std::fs::set_permissions(&cargo_toml, std::fs::Permissions::from_mode(0o000)).unwrap();

        let result = infer_gate_commands(dir.path());

        // Restore read access so the tempdir can clean itself up.
        std::fs::set_permissions(&cargo_toml, std::fs::Permissions::from_mode(0o644)).unwrap();

        if test_runner_is_root() {
            // Root ignores permission bits entirely, so the read above
            // would have succeeded and this test can't exercise the fix —
            // skip rather than fail for a reason unrelated to it.
            return;
        }
        let err = result.unwrap_err();
        assert!(
            err.to_string().contains("Cargo.toml"),
            "expected the real Cargo.toml read error to surface, got: {err}"
        );
    }

    #[cfg(unix)]
    fn test_runner_is_root() -> bool {
        std::env::var("USER").as_deref() == Ok("root") || std::env::var("USER").is_err()
    }

    #[test]
    fn empty_repo_errors_instead_of_proposing_an_empty_gate() {
        let dir = tempfile::tempdir().unwrap();
        let err = infer_gate_commands(dir.path()).unwrap_err();
        assert!(err.to_string().contains("could not infer"));
    }

    #[test]
    fn nonexistent_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(infer_gate_commands(&missing).is_err());
    }

    #[test]
    fn onboard_repo_writes_a_pending_proposal_from_inferred_commands() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "Cargo.toml", "[workspace]\nmembers = [\"a\"]\n");
        let db = Database::open_in_memory().unwrap();

        let config = onboard_repo(&db, "ironrace/ironmem", dir.path()).unwrap();

        assert_eq!(
            config.gate_commands,
            vec!["cargo test --workspace".to_string()]
        );
        assert_eq!(
            config.state,
            super::super::gate_config::GateConfigState::Pending
        );
        assert!(
            !super::super::gate_config::is_gate_config_approved(&db, "ironrace/ironmem").unwrap()
        );
    }

    #[test]
    fn onboard_repo_propagates_inference_failure_without_writing_anything() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open_in_memory().unwrap();

        assert!(onboard_repo(&db, "some/repo", dir.path()).is_err());
        assert!(super::super::gate_config::get_gate_config(&db, "some/repo")
            .unwrap()
            .is_none());
    }
}
