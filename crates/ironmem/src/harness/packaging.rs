//! Packaging coverage checker for the harness registry.
//!
//! Every entry in `REGISTRY` must have a corresponding plugin root directory
//! at `.<id>-plugin/` in the repo root, containing three required assets.
//! [`check_packaging_coverage`] enforces this invariant and is exercised by
//! the drift-lint tests below — a registry addition without matching packaging
//! causes an immediate build-test failure.

use super::HarnessSpec;
use std::path::Path;

/// Required assets inside each harness plugin root.
const REQUIRED_ASSETS: &[&str] = &["bin/ironmem-mcp.sh", "hooks/ironmem-hook.sh", "plugin.json"];

/// Check that every registered harness has its plugin root with required assets.
///
/// For each `spec` in `registry`, this function looks for a directory named
/// `.<id>-plugin/` under `repo_root` and verifies that the following paths
/// exist inside it:
/// - `bin/ironmem-mcp.sh`
/// - `hooks/ironmem-hook.sh`
/// - `plugin.json`
///
/// Returns `Ok(())` when every entry is fully packaged.
/// Returns `Err(messages)` with one human-readable entry per missing root or
/// asset, so the caller can surface all gaps in one pass.
pub fn check_packaging_coverage(
    repo_root: &Path,
    registry: &[HarnessSpec],
) -> Result<(), Vec<String>> {
    let mut missing: Vec<String> = Vec::new();

    for spec in registry {
        let plugin_root = repo_root.join(format!(".{}-plugin", spec.id));

        if !plugin_root.exists() {
            missing.push(format!(
                "harness '{}': missing plugin root {}",
                spec.id,
                plugin_root.display()
            ));
            // No point checking individual assets when the root is absent.
            continue;
        }

        for asset in REQUIRED_ASSETS {
            let asset_path = plugin_root.join(asset);
            if !asset_path.exists() {
                missing.push(format!(
                    "harness '{}': missing {}",
                    spec.id,
                    asset_path.display()
                ));
            }
        }
    }

    if missing.is_empty() {
        Ok(())
    } else {
        Err(missing)
    }
}

// ---------------------------------------------------------------------------
// Drift-lint tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{HarnessSpec, TranscriptParserKind, REGISTRY};

    /// Resolve the repo root from CARGO_MANIFEST_DIR.
    ///
    /// CARGO_MANIFEST_DIR = `<repo>/crates/ironmem`
    /// ancestors: nth(0)=ironmem, nth(1)=crates, nth(2)=repo root
    fn repo_root() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .expect("repo root must be two levels above CARGO_MANIFEST_DIR")
            .to_path_buf()
    }

    /// Production coverage: claude + codex are fully packaged today.
    #[test]
    fn packaging_coverage_passes_for_production_registry() {
        let root = repo_root();
        check_packaging_coverage(&root, REGISTRY).unwrap_or_else(|errs| {
            panic!(
                "packaging drift detected — add the missing assets:\n{}",
                errs.join("\n")
            );
        });
    }

    /// Synthetic-failure: adding a "gemini" harness without its plugin root
    /// must produce a non-empty Err list mentioning "gemini".
    #[test]
    fn packaging_coverage_fails_for_unpackaged_synthetic_harness() {
        const GEMINI_SPEC: HarnessSpec = HarnessSpec {
            id: "gemini",
            display_name: "Gemini CLI",
            binary: "gemini",
            rules_file: "GEMINI.md",
            write_rules_default: false,
            client_info_aliases: &["gemini"],
            env_aliases: &["gemini"],
            additional_context_support: false,
            occupancy_support: false,
            transcript_parser: TranscriptParserKind::None,
        };

        let claude = REGISTRY.iter().find(|s| s.id == "claude").copied().unwrap();
        let codex = REGISTRY.iter().find(|s| s.id == "codex").copied().unwrap();
        let injected = [claude, codex, GEMINI_SPEC];
        let root = repo_root();

        let errs = check_packaging_coverage(&root, &injected)
            .expect_err("must fail when .gemini-plugin/ does not exist");

        assert!(
            !errs.is_empty(),
            "error list must be non-empty for unpackaged harness"
        );
        assert!(
            errs.iter().any(|e| e.contains("gemini")),
            "error list must mention 'gemini'; got: {errs:?}"
        );
    }
}
