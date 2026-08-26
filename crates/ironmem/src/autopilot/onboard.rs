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

    let mut commands = Vec::new();
    if let Some(cmd) = infer_rust(repo_path)? {
        commands.push(cmd);
    }
    if let Some(cmd) = infer_python(repo_path) {
        commands.push(cmd);
    }
    if let Some(cmd) = infer_swift(repo_path) {
        commands.push(cmd);
    }
    if let Some(cmd) = infer_node(repo_path)? {
        commands.push(cmd);
    }
    if commands.is_empty() {
        if let Some(cmd) = infer_makefile_fallback(repo_path)? {
            commands.push(cmd);
        }
    }

    if commands.is_empty() {
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

/// `Cargo.toml` present → Rust. A root `[workspace]` table means member
/// crates typically don't all build/test from the root package alone, so
/// `--workspace` is required for the gate to actually cover them; a plain
/// package gets the simpler `cargo test`.
fn infer_rust(repo_path: &Path) -> Result<Option<String>, MemoryError> {
    let manifest = repo_path.join("Cargo.toml");
    if !manifest.is_file() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&manifest)?;
    let is_workspace = content.lines().any(|line| line.trim() == "[workspace]");
    Ok(Some(if is_workspace {
        "cargo test --workspace".to_string()
    } else {
        "cargo test".to_string()
    }))
}

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
    let content = std::fs::read_to_string(&manifest)?;
    let value: serde_json::Value = serde_json::from_str(&content)?;
    let test_script = value
        .get("scripts")
        .and_then(|scripts| scripts.get("test"))
        .and_then(|script| script.as_str());
    Ok(match test_script {
        Some(script)
            if !script.trim().is_empty() && !script.contains("Error: no test specified") =>
        {
            Some("npm test".to_string())
        }
        _ => None,
    })
}

/// Fallback consulted only when no other stack was recognized (see module
/// docs): a `Makefile` with a line starting `test:` → `make test`.
fn infer_makefile_fallback(repo_path: &Path) -> Result<Option<String>, MemoryError> {
    let makefile = repo_path.join("Makefile");
    if !makefile.is_file() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&makefile)?;
    let has_test_target = content.lines().any(|line| line.starts_with("test:"));
    Ok(has_test_target.then(|| "make test".to_string()))
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
