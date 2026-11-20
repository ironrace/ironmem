//! Packaging coverage checker for the harness registry.
//!
//! Every entry in `REGISTRY` must have a corresponding plugin root directory
//! at `.<id>-plugin/` in the repo root, containing its required assets — and,
//! for a harness whose hooks the installer wires instead
//! (`plugin_ships_hooks: false`), no hook wiring at all.
//! [`check_packaging_coverage`] enforces this invariant and is exercised by
//! the drift-lint tests below — a registry addition without matching packaging
//! causes an immediate build-test failure.

use super::HarnessSpec;
use std::path::Path;

/// Required assets inside each harness plugin root.
const REQUIRED_ASSETS: &[&str] = &["bin/ironmem-mcp.sh", "plugin.json"];

/// The plugin's hook wrapper, required when the spec's `plugin_ships_hooks`.
const HOOK_ASSET: &str = "hooks/ironmem-hook.sh";

/// Hook wiring a plugin must NOT carry when its hooks come from the
/// installer: the wrapper directory and a native `hooks.json` (the file
/// Antigravity loads from a plugin root).
const FORBIDDEN_WHEN_INSTALLER_HOOKS: &[&str] = &["hooks", "hooks.json"];

/// Check that every registered harness has its plugin root with required assets.
///
/// For each `spec` in `registry`, this function looks for a directory named
/// `.<id>-plugin/` under `repo_root` and verifies that the following paths
/// exist inside it:
/// - `bin/ironmem-mcp.sh`
/// - `plugin.json`
/// - `hooks/ironmem-hook.sh`, when `spec.plugin_ships_hooks`
///
/// When `spec.plugin_ships_hooks` is `false`, `hooks/` and `hooks.json` must
/// be absent instead, so the plugin and the installer never both fire a hook.
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

        let hook_asset = spec.plugin_ships_hooks.then_some(HOOK_ASSET);
        for asset in REQUIRED_ASSETS.iter().copied().chain(hook_asset) {
            let asset_path = plugin_root.join(asset);
            if !asset_path.exists() {
                missing.push(format!(
                    "harness '{}': missing {}",
                    spec.id,
                    asset_path.display()
                ));
            }
        }
        if !spec.plugin_ships_hooks {
            for wiring in FORBIDDEN_WHEN_INSTALLER_HOOKS {
                let path = plugin_root.join(wiring);
                if path.exists() {
                    missing.push(format!(
                        "harness '{}': {} must not exist (its hooks come from scripts/install-ironmem.sh, so a plugin copy would fire them twice)",
                        spec.id,
                        path.display()
                    ));
                }
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

    /// Production coverage: every registry harness is fully packaged, and agy
    /// (installer-wired hooks) ships no hook wiring.
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

    /// A plugin root that still carries hook wiring fails for a harness whose
    /// hooks the installer owns, and passes once the wiring is removed; the
    /// same root with `plugin_ships_hooks: true` instead requires the wrapper.
    #[test]
    fn packaging_forbids_plugin_hooks_when_the_installer_owns_them() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".zeta-plugin");
        std::fs::create_dir_all(root.join("bin")).unwrap();
        std::fs::write(root.join("bin/ironmem-mcp.sh"), "").unwrap();
        std::fs::write(root.join("plugin.json"), "{}").unwrap();
        std::fs::write(root.join("hooks.json"), "{}").unwrap();
        let mcp_only = HarnessSpec {
            id: "zeta",
            plugin_ships_hooks: false,
            installer_registers: false,
            ..REGISTRY[0]
        };

        let errs = check_packaging_coverage(dir.path(), &[mcp_only]).unwrap_err();
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("hooks.json must not exist"), "{errs:?}");

        std::fs::remove_file(root.join("hooks.json")).unwrap();
        check_packaging_coverage(dir.path(), &[mcp_only]).expect("MCP-only root passes");

        let with_hooks = HarnessSpec {
            plugin_ships_hooks: true,
            installer_registers: false,
            ..mcp_only
        };
        let errs = check_packaging_coverage(dir.path(), &[with_hooks]).unwrap_err();
        assert!(
            errs.iter().any(|e| e.contains("hooks/ironmem-hook.sh")),
            "{errs:?}"
        );
    }

    /// Synthetic-failure: adding an unpackaged harness (one with no matching
    /// `.<id>-plugin/` root) must produce a non-empty Err list naming it.
    /// Uses a placeholder id that is guaranteed absent from the repo root.
    #[test]
    fn packaging_coverage_fails_for_unpackaged_synthetic_harness() {
        const UNPACKAGED_SPEC: HarnessSpec = HarnessSpec {
            id: "totally-unpackaged-harness",
            display_name: "Totally Unpackaged Harness",
            binary: "totally-unpackaged-harness",
            rules_file: "UNPACKAGED.md",
            rules_strategy: crate::harness::RulesStrategy::Import {
                directive: "@./AGENTS.md",
            },
            write_rules_default: false,
            client_info_aliases: &["totally-unpackaged-harness"],
            env_aliases: &["totally-unpackaged-harness"],
            additional_context_support: false,
            occupancy_support: false,
            transcript_parser: TranscriptParserKind::None,
            hook_output: crate::harness::HookOutputFormat::HookResponse,
            prompt_flag: None,
            deprecated_aliases: &[],
            plugin_ships_hooks: true,
            installer_registers: false,
        };

        let claude = REGISTRY.iter().find(|s| s.id == "claude").copied().unwrap();
        let codex = REGISTRY.iter().find(|s| s.id == "codex").copied().unwrap();
        let injected = [claude, codex, UNPACKAGED_SPEC];
        let root = repo_root();

        let errs = check_packaging_coverage(&root, &injected)
            .expect_err("must fail when .totally-unpackaged-harness-plugin/ does not exist");

        assert!(
            !errs.is_empty(),
            "error list must be non-empty for unpackaged harness"
        );
        assert!(
            errs.iter()
                .any(|e| e.contains("totally-unpackaged-harness")),
            "error list must mention the unpackaged harness id; got: {errs:?}"
        );
    }
}
