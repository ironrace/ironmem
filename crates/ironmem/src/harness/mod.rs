//! Harness registry — canonical descriptions of supported AI assistant harnesses.
//!
//! The `REGISTRY` constant is the single source of truth for harness identifiers,
//! binaries, rules files, client-info aliases, and capability flags.  All
//! lookup helpers take an explicit `registry: &[HarnessSpec]` slice so they
//! work on injected test slices without global-state mutation.

mod packaging;
pub use packaging::check_packaging_coverage;

/// Canonical rules file and single source of truth for dependent (non-native)
/// harness rules files. `Native` strategies target this file directly; `Import`
/// and `Copy` strategies derive their content from it.
pub const CANONICAL_RULES_FILE: &str = "AGENTS.md";

/// How a harness encodes session transcripts (used by the abeval token parser).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TranscriptParserKind {
    Claude,
    Codex,
    None,
}

/// Strategy used to populate a harness rules file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RulesStrategy {
    Native,
    Import { directive: &'static str },
    Copy,
}

impl RulesStrategy {
    pub(crate) fn as_text(&self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Import { .. } => "import",
            Self::Copy => "copy",
        }
    }
}

/// A validated lowercase slug (`[a-z0-9][a-z0-9_-]*`).
///
/// Construction fails on empty strings, uppercase letters, whitespace, or
/// characters outside the allowed set.  The stored `&'static str` values in
/// [`REGISTRY`] are trusted as already valid and bypass this check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HarnessId(&'static str);

impl HarnessId {
    /// Validate a slug without storing it.
    ///
    /// Use this to check runtime strings (e.g. from `IRONMEM_HARNESS` or a
    /// config file) that the caller owns.  No allocation is required — the
    /// borrow is released when this function returns.
    ///
    /// # Errors
    ///
    /// Returns `Err` with a description when `s` does not match
    /// `[a-z0-9][a-z0-9_-]*`.
    pub fn validate(s: &str) -> Result<(), String> {
        if Self::is_valid_slug(s) {
            Ok(())
        } else {
            Err(format!(
                "invalid harness slug {:?}: must match [a-z0-9][a-z0-9_-]*",
                s
            ))
        }
    }

    /// Construct a `HarnessId` from a `&'static str`, validating slug format.
    ///
    /// # Errors
    ///
    /// Returns `Err(s)` with the offending string if validation fails.
    pub fn new(s: &'static str) -> Result<Self, &'static str> {
        if Self::is_valid_slug(s) {
            Ok(Self(s))
        } else {
            Err(s)
        }
    }

    /// Construct a `HarnessId` from a trusted `&'static str` without
    /// validation.  Use only for compile-time-known values (e.g. registry
    /// constants) where the slug is guaranteed correct.
    ///
    /// Crate-internal: callers MUST pass a compile-time constant slug. This
    /// bypasses [`Self::is_valid_slug`], so a leaked runtime string would
    /// silently skip validation. Route untrusted input through
    /// [`Self::validate`] / [`Self::new`] instead.
    pub(crate) const fn new_unchecked(s: &'static str) -> Self {
        Self(s)
    }

    /// Return the inner string slice.
    pub fn as_str(self) -> &'static str {
        self.0
    }

    // ---- private -----------------------------------------------------------

    fn is_valid_slug(s: &str) -> bool {
        if s.is_empty() {
            return false;
        }
        let mut chars = s.chars();
        // First char: [a-z0-9]
        match chars.next() {
            Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
            _ => return false,
        }
        // Remaining chars: [a-z0-9_-]
        for c in chars {
            if !c.is_ascii_lowercase() && !c.is_ascii_digit() && c != '_' && c != '-' {
                return false;
            }
        }
        true
    }
}

/// Static description of a single supported harness.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct HarnessSpec {
    /// Canonical lowercase identifier (e.g. `"claude"`, `"codex"`).
    pub id: &'static str,
    /// Human-readable display name (e.g. `"Claude Code"`).
    pub display_name: &'static str,
    /// Executable name looked up on `PATH` (e.g. `"claude"`).
    pub binary: &'static str,
    /// Rules file written by `ironmem write-rules` for this harness.
    pub rules_file: &'static str,
    /// How this harness's rules file should be hydrated/treated.
    pub rules_strategy: RulesStrategy,
    /// Whether this harness is included in the default `write-rules` run.
    pub write_rules_default: bool,
    /// Substrings matched against a lowercased MCP `clientInfo.name` value to
    /// identify this harness from the `initialize` request.
    pub client_info_aliases: &'static [&'static str],
    /// Values accepted by `IRONMEM_HARNESS` that map to this harness.
    pub env_aliases: &'static [&'static str],
    /// Whether the harness supports `hookSpecificOutput.additionalContext`.
    pub additional_context_support: bool,
    /// Whether the harness emits occupancy samples that ironmem can capture.
    pub occupancy_support: bool,
    /// Which transcript parser to use when measuring token counts.
    pub transcript_parser: TranscriptParserKind,
}

/// All known harnesses.  Add new entries here; never hard-code harness ids
/// elsewhere in the codebase.
pub const REGISTRY: &[HarnessSpec] = &[
    HarnessSpec {
        id: "claude",
        display_name: "Claude Code",
        binary: "claude",
        rules_file: "CLAUDE.md",
        rules_strategy: RulesStrategy::Import {
            directive: "@AGENTS.md",
        },
        write_rules_default: true,
        client_info_aliases: &["claude", "claude-code"],
        env_aliases: &["claude", "claude-code"],
        additional_context_support: true,
        occupancy_support: true,
        transcript_parser: TranscriptParserKind::Claude,
    },
    HarnessSpec {
        id: "codex",
        display_name: "Codex",
        binary: "codex",
        rules_file: "AGENTS.md",
        rules_strategy: RulesStrategy::Native,
        write_rules_default: true,
        client_info_aliases: &["codex"],
        env_aliases: &["codex"],
        additional_context_support: false,
        occupancy_support: true,
        transcript_parser: TranscriptParserKind::Codex,
    },
    HarnessSpec {
        id: "grok",
        display_name: "Grok",
        binary: "grok",
        rules_file: "GROK.md",
        rules_strategy: RulesStrategy::Import {
            directive: "@AGENTS.md",
        },
        // Scaffolding only (#190 Task 11): not yet a default write-rules
        // target, and its `.grok-plugin/` packaging is a minimal stand-in —
        // real Grok CLI integration lands separately.
        write_rules_default: false,
        client_info_aliases: &["grok"],
        env_aliases: &["grok"],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    },
    HarnessSpec {
        id: "gemini",
        display_name: "Gemini CLI",
        binary: "gemini",
        rules_file: "GEMINI.md",
        rules_strategy: RulesStrategy::Import {
            directive: "@AGENTS.md",
        },
        // Scaffolding only (#190 Task 11): not yet a default write-rules
        // target, and its `.gemini-plugin/` packaging is a minimal stand-in —
        // real Gemini CLI integration lands separately.
        write_rules_default: false,
        client_info_aliases: &["gemini"],
        env_aliases: &["gemini"],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    },
];

// ---------------------------------------------------------------------------
// Registry serialization helpers
// ---------------------------------------------------------------------------

/// Serialize the registry as pretty JSON (including strategy via rules_strategy),
/// used by `ironmem harnesses --format=json` and packaging drift-lint.
pub fn registry_json(registry: &[HarnessSpec]) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(registry)
}

/// Human-readable one-line-per-harness listing.
///
/// Format: `{id}  {display_name}  rules={rules_file}  strategy={strategy}  binary={binary}`
pub fn registry_text(registry: &[HarnessSpec]) -> String {
    registry
        .iter()
        .map(|s| {
            format!(
                "{}  {}  rules={}  strategy={}  binary={}\n",
                s.id,
                s.display_name,
                s.rules_file,
                s.rules_strategy.as_text(),
                s.binary
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Lookup helpers
// ---------------------------------------------------------------------------

/// Find a harness by exact `id` match.
pub fn by_id<'r>(id: &str, registry: &'r [HarnessSpec]) -> Option<&'r HarnessSpec> {
    registry.iter().find(|s| s.id == id)
}

/// Identify a harness from an MCP `clientInfo.name` value.
///
/// The `name` is lowercased before matching; returns the `id` of the first
/// harness whose `client_info_aliases` contains a substring found in `name`.
pub fn classify_client_info<'r>(name: &str, registry: &'r [HarnessSpec]) -> Option<&'r str> {
    let lower = name.to_ascii_lowercase();
    for spec in registry {
        for alias in spec.client_info_aliases {
            if lower.contains(alias) {
                return Some(spec.id);
            }
        }
    }
    None
}

/// Canonicalize an `IRONMEM_HARNESS`-style input string to a harness id.
///
/// Returns the `id` if `input` exactly matches any `env_alias`.
pub fn canonicalize_input<'r>(input: &str, registry: &'r [HarnessSpec]) -> Option<&'r str> {
    for spec in registry {
        for alias in spec.env_aliases {
            if *alias == input {
                return Some(spec.id);
            }
        }
    }
    None
}

/// Return the `rules_file` for every harness where `write_rules_default` is `true`.
///
/// Validation uses `rules_file_entries` so conflicting strategies for the same
/// `rules_file` are rejected instead of being silently collapsed.
pub fn default_rules_targets(registry: &[HarnessSpec]) -> Result<Vec<&'static str>, String> {
    rules_file_entries(registry)?;

    let mut targets = Vec::new();
    for spec in registry.iter().filter(|s| s.write_rules_default) {
        if !targets.contains(&spec.rules_file) {
            targets.push(spec.rules_file);
        }
    }
    Ok(targets)
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct RulesFileEntry {
    pub rules_file: &'static str,
    pub rules_strategy: RulesStrategy,
}

/// Resolve the registry into de-duplicated rule-file targets and enforce strategy
/// invariants.
///
/// - `rules_file` values are deduplicated by filename only when the associated
///   `rules_strategy` matches.
/// - `rules_strategy::Native` is valid only for `AGENTS.md`.
/// - Non-native strategies are invalid for `AGENTS.md`.
pub(crate) fn rules_file_entries(registry: &[HarnessSpec]) -> Result<Vec<RulesFileEntry>, String> {
    let mut entries: Vec<RulesFileEntry> = Vec::new();

    for spec in registry {
        match spec.rules_strategy {
            RulesStrategy::Native => {
                if spec.rules_file != CANONICAL_RULES_FILE {
                    return Err(format!(
                        "invalid rules strategy for '{}': Native strategy requires AGENTS.md",
                        spec.id
                    ));
                }
            }
            RulesStrategy::Import { .. } | RulesStrategy::Copy => {
                if spec.rules_file == CANONICAL_RULES_FILE {
                    return Err(format!(
                        "invalid rules strategy for '{}': non-native strategies cannot target AGENTS.md",
                        spec.id
                    ));
                }
            }
        }

        let existing = entries.iter_mut().find(|e| e.rules_file == spec.rules_file);
        match existing {
            Some(existing) => {
                if existing.rules_strategy != spec.rules_strategy {
                    return Err(format!(
                        "conflicting rules_strategy for '{}': {:?} and {:?}",
                        spec.rules_file, existing.rules_strategy, spec.rules_strategy
                    ));
                }
            }
            None => entries.push(RulesFileEntry {
                rules_file: spec.rules_file,
                rules_strategy: spec.rules_strategy,
            }),
        }
    }

    Ok(entries)
}

// ---------------------------------------------------------------------------
// Proxy MCP registration helper (#190 Task 11)
// ---------------------------------------------------------------------------

/// Build the canonical `--connect` proxy MCP command args every harness
/// should register: `["serve", "--connect", <default daemon socket path>]`.
///
/// Deliberately does NOT branch on `_harness_id`: every harness gets back the
/// exact same args for the same `config`, using `Config::daemon_socket_path`
/// (the same default the `--listen` daemon binds and the auto-spawn path
/// spawns against — see `crate::config::Config` and `crate::mcp::daemon`).
/// `_harness_id` is accepted anyway so every registration call site
/// (`ensure_claude_registered`, `ensure_codex_registered`, and future
/// grok/gemini launchers, #190 Task 12/13) threads the harness it is
/// registering through this one seam — a future regression that
/// accidentally introduces per-harness variation changes this function's
/// signature/behavior in an obvious, reviewable way, and is caught by
/// `proxy_command_args_identical_for_every_registry_harness` below.
pub fn proxy_command_args(_harness_id: &str, config: &crate::config::Config) -> Vec<String> {
    vec![
        "serve".to_string(),
        "--connect".to_string(),
        config.daemon_socket_path().display().to_string(),
    ]
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- Registry contents -------------------------------------------------

    #[test]
    fn claude_spec_values_match_current_hardcoded_values() {
        let spec = by_id("claude", REGISTRY).expect("claude must be in REGISTRY");
        assert_eq!(spec.id, "claude");
        assert_eq!(spec.display_name, "Claude Code");
        assert_eq!(spec.binary, "claude");
        assert_eq!(spec.rules_file, "CLAUDE.md");
        assert_eq!(
            spec.rules_strategy,
            RulesStrategy::Import {
                directive: "@AGENTS.md"
            }
        );
        assert!(spec.write_rules_default);
        assert_eq!(spec.client_info_aliases, &["claude", "claude-code"]);
        assert_eq!(spec.env_aliases, &["claude", "claude-code"]);
        assert!(spec.additional_context_support);
        assert!(spec.occupancy_support);
        assert_eq!(spec.transcript_parser, TranscriptParserKind::Claude);
    }

    #[test]
    fn codex_spec_values_match_current_hardcoded_values() {
        let spec = by_id("codex", REGISTRY).expect("codex must be in REGISTRY");
        assert_eq!(spec.id, "codex");
        assert_eq!(spec.display_name, "Codex");
        assert_eq!(spec.binary, "codex");
        assert_eq!(spec.rules_file, "AGENTS.md");
        assert_eq!(spec.rules_strategy, RulesStrategy::Native);
        assert!(spec.write_rules_default);
        assert_eq!(spec.client_info_aliases, &["codex"]);
        assert_eq!(spec.env_aliases, &["codex"]);
        assert!(!spec.additional_context_support);
        assert!(spec.occupancy_support);
        assert_eq!(spec.transcript_parser, TranscriptParserKind::Codex);
    }

    // ---- HarnessId construction --------------------------------------------

    #[test]
    fn harness_id_rejects_uppercase() {
        assert!(HarnessId::new("GEMINI").is_err());
    }

    #[test]
    fn harness_id_rejects_whitespace() {
        assert!(HarnessId::new("gemini harness").is_err());
    }

    #[test]
    fn harness_id_rejects_empty() {
        assert!(HarnessId::new("").is_err());
    }

    #[test]
    fn harness_id_accepts_valid_slug() {
        let id = HarnessId::new("gemini").expect("'gemini' is a valid slug");
        assert_eq!(id.as_str(), "gemini");
    }

    #[test]
    fn harness_id_accepts_slug_with_dash_and_digit() {
        let id = HarnessId::new("gemini-2").expect("'gemini-2' is a valid slug");
        assert_eq!(id.as_str(), "gemini-2");
    }

    #[test]
    fn harness_id_accepts_underscore_slug() {
        let id = HarnessId::new("grok_ai").expect("'grok_ai' is a valid slug");
        assert_eq!(id.as_str(), "grok_ai");
    }

    #[test]
    fn validate_accepts_runtime_string() {
        let runtime = String::from("grok_ai");
        assert!(HarnessId::validate(&runtime).is_ok());
    }

    #[test]
    fn validate_rejects_invalid_runtime_string() {
        let runtime = String::from("UPPERCASE");
        let err = HarnessId::validate(&runtime).unwrap_err();
        assert!(err.contains("invalid harness slug"));
    }

    // ---- Injected 3-entry registry ----------------------------------------

    const GEMINI_SPEC: HarnessSpec = HarnessSpec {
        id: "gemini",
        display_name: "Gemini CLI",
        binary: "gemini",
        rules_file: "GEMINI.md",
        rules_strategy: RulesStrategy::Import {
            directive: "@./AGENTS.md",
        },
        write_rules_default: false,
        client_info_aliases: &["gemini"],
        env_aliases: &["gemini"],
        additional_context_support: false,
        occupancy_support: false,
        transcript_parser: TranscriptParserKind::None,
    };

    fn three_entry_registry() -> [HarnessSpec; 3] {
        [REGISTRY[0], REGISTRY[1], GEMINI_SPEC]
    }

    #[test]
    fn injected_registry_resolves_synthetic_gemini() {
        let reg = three_entry_registry();
        let spec = by_id("gemini", &reg).expect("gemini must resolve in injected registry");
        assert_eq!(spec.id, "gemini");
        assert_eq!(spec.binary, "gemini");
    }

    #[test]
    fn injected_registry_still_resolves_claude_and_codex() {
        let reg = three_entry_registry();
        assert!(by_id("claude", &reg).is_some());
        assert!(by_id("codex", &reg).is_some());
    }

    // ---- classify_client_info ---------------------------------------------

    #[test]
    fn classify_client_info_codex_cli() {
        assert_eq!(classify_client_info("codex-cli", REGISTRY), Some("codex"));
    }

    #[test]
    fn classify_client_info_claude_code() {
        assert_eq!(
            classify_client_info("claude-code", REGISTRY),
            Some("claude")
        );
    }

    #[test]
    fn classify_client_info_unknown() {
        assert_eq!(classify_client_info("unknown-tool", REGISTRY), None);
    }

    #[test]
    fn classify_client_info_gemini_in_injected_registry() {
        let reg = three_entry_registry();
        assert_eq!(classify_client_info("gemini", &reg), Some("gemini"));
    }

    #[test]
    fn classify_client_info_case_insensitive() {
        // MCP clients may send mixed-case names
        assert_eq!(
            classify_client_info("Claude-Code", REGISTRY),
            Some("claude")
        );
        assert_eq!(classify_client_info("CODEX", REGISTRY), Some("codex"));
    }

    // ---- canonicalize_input -----------------------------------------------

    #[test]
    fn canonicalize_input_codex() {
        assert_eq!(canonicalize_input("codex", REGISTRY), Some("codex"));
    }

    #[test]
    fn canonicalize_input_claude_code_alias() {
        assert_eq!(canonicalize_input("claude-code", REGISTRY), Some("claude"));
    }

    #[test]
    fn canonicalize_input_unknown() {
        assert_eq!(canonicalize_input("unknown", REGISTRY), None);
    }

    // ---- default_rules_targets --------------------------------------------

    #[test]
    fn default_rules_targets_returns_both_files() {
        let targets = default_rules_targets(REGISTRY).expect("default targets should be valid");
        assert!(
            targets.contains(&"CLAUDE.md"),
            "expected CLAUDE.md in targets"
        );
        assert!(
            targets.contains(&"AGENTS.md"),
            "expected AGENTS.md in targets"
        );
    }

    #[test]
    fn default_rules_targets_excludes_non_default_entries() {
        let reg = three_entry_registry();
        let targets =
            default_rules_targets(&reg).expect("default targets should ignore non-default");
        assert!(
            !targets.contains(&"GEMINI.md"),
            "GEMINI.md should be excluded"
        );
        assert_eq!(targets.len(), 2);
    }

    #[test]
    fn default_rules_targets_rejects_conflicting_strategies() {
        let alpha = HarnessSpec {
            id: "alpha",
            display_name: "Alpha",
            binary: "alpha",
            rules_file: "CLAUDE.md",
            rules_strategy: RulesStrategy::Import {
                directive: "@AGENTS.md",
            },
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: true,
            occupancy_support: true,
            transcript_parser: TranscriptParserKind::None,
        };
        let beta = HarnessSpec {
            id: "beta",
            display_name: "Beta",
            binary: "beta",
            rules_file: "CLAUDE.md",
            rules_strategy: RulesStrategy::Copy,
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: true,
            occupancy_support: true,
            transcript_parser: TranscriptParserKind::None,
        };

        let err = default_rules_targets(&[alpha, beta]).unwrap_err();
        assert!(
            err.contains("conflicting rules_strategy"),
            "expected conflict error, got: {err}"
        );
    }

    #[test]
    fn rules_file_entries_allow_duplicate_native_agents_files() {
        let alpha = HarnessSpec {
            id: "alpha",
            display_name: "Alpha",
            binary: "alpha",
            rules_file: "AGENTS.md",
            rules_strategy: RulesStrategy::Native,
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: true,
            occupancy_support: true,
            transcript_parser: TranscriptParserKind::None,
        };
        let beta = HarnessSpec {
            id: "beta",
            display_name: "Beta",
            binary: "beta",
            rules_file: "AGENTS.md",
            rules_strategy: RulesStrategy::Native,
            write_rules_default: false,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: true,
            occupancy_support: true,
            transcript_parser: TranscriptParserKind::None,
        };

        let entries =
            rules_file_entries(&[alpha, beta]).expect("entries should dedupe native duplicates");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].rules_file, "AGENTS.md");
        assert_eq!(entries[0].rules_strategy, RulesStrategy::Native);
    }

    #[test]
    fn rules_file_entries_reject_conflicting_strategies_for_same_rules_file() {
        let alpha = HarnessSpec {
            id: "alpha",
            display_name: "Alpha",
            binary: "alpha",
            rules_file: "CLAUDE.md",
            rules_strategy: RulesStrategy::Import {
                directive: "@AGENTS.md",
            },
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: true,
            occupancy_support: true,
            transcript_parser: TranscriptParserKind::None,
        };
        let beta = HarnessSpec {
            id: "beta",
            display_name: "Beta",
            binary: "beta",
            rules_file: "CLAUDE.md",
            rules_strategy: RulesStrategy::Copy,
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: true,
            occupancy_support: true,
            transcript_parser: TranscriptParserKind::None,
        };

        let err = rules_file_entries(&[alpha, beta]).unwrap_err();
        assert!(
            err.contains("conflicting rules_strategy"),
            "expected conflict error, got: {err}"
        );
    }

    fn spec_with(rules_file: &'static str, rules_strategy: RulesStrategy) -> HarnessSpec {
        HarnessSpec {
            id: "probe",
            display_name: "Probe",
            binary: "probe",
            rules_file,
            rules_strategy,
            write_rules_default: true,
            client_info_aliases: &[],
            env_aliases: &[],
            additional_context_support: false,
            occupancy_support: false,
            transcript_parser: TranscriptParserKind::None,
        }
    }

    #[test]
    fn rules_file_entries_reject_native_strategy_on_non_agents_file() {
        // Native is the canonical-file strategy: it must target AGENTS.md only.
        let spec = spec_with("CLAUDE.md", RulesStrategy::Native);
        let err = rules_file_entries(&[spec]).unwrap_err();
        assert!(
            err.contains("Native strategy requires AGENTS.md"),
            "expected native-requires-AGENTS error, got: {err}"
        );
    }

    #[test]
    fn rules_file_entries_reject_non_native_strategy_on_agents_file() {
        // AGENTS.md is the source of truth; dependent strategies cannot own it.
        let import = spec_with(
            CANONICAL_RULES_FILE,
            RulesStrategy::Import {
                directive: "@AGENTS.md",
            },
        );
        let err = rules_file_entries(&[import]).unwrap_err();
        assert!(
            err.contains("non-native strategies cannot target AGENTS.md"),
            "expected import-cannot-target-AGENTS error, got: {err}"
        );

        let copy = spec_with(CANONICAL_RULES_FILE, RulesStrategy::Copy);
        let err = rules_file_entries(&[copy]).unwrap_err();
        assert!(
            err.contains("non-native strategies cannot target AGENTS.md"),
            "expected copy-cannot-target-AGENTS error, got: {err}"
        );
    }

    #[test]
    fn every_registry_id_is_a_valid_harness_slug() {
        // The registry stores ids as bare &'static str and bypasses HarnessId
        // validation via new_unchecked; this test is the guard that a typo'd or
        // uppercase id never ships undetected.
        for spec in REGISTRY {
            assert!(
                HarnessId::validate(spec.id).is_ok(),
                "REGISTRY id {:?} must be a valid harness slug",
                spec.id
            );
        }
    }

    // ---- registry_json / registry_text ------------------------------------

    #[test]
    fn registry_json_parses_as_four_entry_array() {
        let json = registry_json(REGISTRY).expect("serialization must succeed");
        let val: serde_json::Value =
            serde_json::from_str(&json).expect("output must be valid JSON");
        let arr = val.as_array().expect("top-level must be an array");
        assert_eq!(arr.len(), 4, "claude, codex, grok, gemini (#190 Task 11)");
        let ids: Vec<&str> = arr.iter().filter_map(|e| e["id"].as_str()).collect();
        assert!(ids.contains(&"claude"), "must contain claude entry");
        assert!(ids.contains(&"codex"), "must contain codex entry");
        assert!(ids.contains(&"grok"), "must contain grok entry");
        assert!(ids.contains(&"gemini"), "must contain gemini entry");
    }

    #[test]
    fn registry_json_entries_have_required_fields() {
        let json = registry_json(REGISTRY).expect("serialization must succeed");
        let arr: serde_json::Value = serde_json::from_str(&json).unwrap();
        for entry in arr.as_array().unwrap() {
            assert!(entry["id"].is_string(), "id must be a string");
            assert!(
                entry["display_name"].is_string(),
                "display_name must be a string"
            );
            assert!(
                entry["rules_file"].is_string(),
                "rules_file must be a string"
            );
            assert!(entry["binary"].is_string(), "binary must be a string");
            assert!(
                entry["write_rules_default"].is_boolean(),
                "write_rules_default must be bool"
            );
            assert!(entry["additional_context_support"].is_boolean());
            assert!(entry["occupancy_support"].is_boolean());
            assert!(
                entry["rules_strategy"].is_object(),
                "rules_strategy must be an object"
            );
            assert!(
                entry["transcript_parser"].is_string(),
                "transcript_parser must be a string"
            );
        }
    }

    #[test]
    fn registry_json_includes_tagged_rules_strategy() {
        let json = registry_json(REGISTRY).expect("serialization must succeed");
        let arr: serde_json::Value = serde_json::from_str(&json).unwrap();
        let mut codex = None;
        let mut claude = None;
        for entry in arr.as_array().unwrap() {
            match entry["id"].as_str() {
                Some("codex") => codex = Some(entry["rules_strategy"].clone()),
                Some("claude") => claude = Some(entry["rules_strategy"].clone()),
                _ => {}
            }
        }

        let codex = codex.expect("codex entry must exist");
        let claude = claude.expect("claude entry must exist");

        assert_eq!(codex["kind"].as_str(), Some("native"));
        assert_eq!(codex.as_object().map(|o| o.len()), Some(1));
        assert!(
            codex.get("directive").is_none(),
            "native strategy must not include directive"
        );
        assert_eq!(codex["kind"].as_str(), Some("native"));
        assert_eq!(claude["kind"].as_str(), Some("import"));
        assert_eq!(claude["directive"].as_str(), Some("@AGENTS.md"));
    }

    #[test]
    fn registry_json_transcript_parser_uses_lowercase() {
        let json = registry_json(REGISTRY).expect("serialization must succeed");
        let arr: serde_json::Value = serde_json::from_str(&json).unwrap();
        let parsers: Vec<&str> = arr
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["transcript_parser"].as_str())
            .collect();
        assert!(
            parsers.contains(&"claude"),
            "claude parser must serialize as 'claude'"
        );
        assert!(
            parsers.contains(&"codex"),
            "codex parser must serialize as 'codex'"
        );
    }

    #[test]
    fn registry_text_contains_claude_and_codex() {
        let text = registry_text(REGISTRY);
        assert!(text.contains("claude"), "text must mention claude");
        assert!(text.contains("codex"), "text must mention codex");
        assert!(text.contains("CLAUDE.md"), "text must mention CLAUDE.md");
        assert!(text.contains("AGENTS.md"), "text must mention AGENTS.md");
        assert!(
            text.contains("strategy=import"),
            "text must include strategy=import"
        );
        assert!(
            text.contains("strategy=native"),
            "text must include strategy=native"
        );
    }

    #[test]
    fn copy_strategy_serializes_as_tagged_copy_in_json_and_text() {
        // Copy is absent from the production REGISTRY, so its wire/text shape is
        // otherwise untested; a rename of the serde tag or as_text arm must fail here.
        let copy = spec_with("COPY.md", RulesStrategy::Copy);

        let json = registry_json(&[copy]).expect("serialization must succeed");
        let arr: serde_json::Value = serde_json::from_str(&json).unwrap();
        let strategy = arr.as_array().unwrap()[0]["rules_strategy"].clone();
        assert_eq!(strategy["kind"].as_str(), Some("copy"));
        assert_eq!(
            strategy.as_object().map(|o| o.len()),
            Some(1),
            "copy strategy must carry no directive field"
        );
        assert!(strategy.get("directive").is_none());

        let text = registry_text(&[copy]);
        assert!(
            text.contains("strategy=copy"),
            "text must include strategy=copy; got: {text}"
        );
    }

    #[test]
    fn registry_json_three_entry_registry_has_gemini() {
        let reg = three_entry_registry();
        let json = registry_json(&reg).expect("serialization must succeed");
        let arr: serde_json::Value = serde_json::from_str(&json).unwrap();
        let arr = arr.as_array().unwrap();
        assert_eq!(
            arr.len(),
            3,
            "three-entry registry must produce 3 JSON objects"
        );
        let ids: Vec<&str> = arr.iter().filter_map(|e| e["id"].as_str()).collect();
        assert!(ids.contains(&"gemini"), "must contain gemini entry");
        assert!(ids.contains(&"claude"), "must still contain claude entry");
        assert!(ids.contains(&"codex"), "must still contain codex entry");
    }

    // ---- Issue #155 end-to-end synthetic third-harness acceptance test ------

    /// Issue #155 acceptance: "third registered harness end-to-end at unit level."
    ///
    /// Registers a synthetic "gemini" harness via an injected 3-entry registry
    /// slice — no production registry or Gemini/Grok/Copilot code is touched.
    /// Exercises all four registry-driven surfaces in one flow:
    ///   (1) attribution (classify_client_info + canonicalize_input)
    ///   (2) write-rules target resolution (resolve_write_targets)
    ///   (3) metrics-row persistence (token_usage INSERT → SELECT round-trip)
    ///   (4) launcher arg-build (launch_invocation from spec)
    #[test]
    fn e2e_synthetic_third_harness_flows_through_all_registry_surfaces() {
        // Synthetic spec — lives in test scope only; NOT added to production REGISTRY.
        const GEMINI_E2E: HarnessSpec = HarnessSpec {
            id: "gemini",
            display_name: "Gemini",
            binary: "gemini",
            rules_file: "GEMINI.md",
            rules_strategy: RulesStrategy::Import {
                directive: "@./AGENTS.md",
            },
            write_rules_default: false,
            client_info_aliases: &["gemini", "gemini-cli"],
            env_aliases: &["gemini"],
            additional_context_support: true,
            occupancy_support: true,
            transcript_parser: TranscriptParserKind::None,
        };

        // Build 3-entry registry; look up by id, never by index, so the test
        // is immune to future REGISTRY reordering.
        let claude_spec = REGISTRY
            .iter()
            .find(|s| s.id == "claude")
            .copied()
            .expect("claude must be in REGISTRY");
        let codex_spec = REGISTRY
            .iter()
            .find(|s| s.id == "codex")
            .copied()
            .expect("codex must be in REGISTRY");
        let registry = [claude_spec, codex_spec, GEMINI_E2E];

        // ── (1) Attribution ──────────────────────────────────────────────────
        assert_eq!(
            classify_client_info("gemini-cli", &registry),
            Some("gemini"),
            "classify_client_info must match 'gemini-cli' substring to gemini"
        );
        assert_eq!(
            canonicalize_input("gemini", &registry),
            Some("gemini"),
            "canonicalize_input must map 'gemini' env alias to id"
        );

        // ── (2) Write-rules target resolution ────────────────────────────────
        let targets = crate::write_rules::resolve_write_targets(None, Some("gemini"), &registry)
            .expect("gemini harness must resolve to its rules_file in injected registry");
        assert_eq!(
            targets,
            vec!["GEMINI.md"],
            "resolve_write_targets must return GEMINI.md for gemini harness"
        );

        // ── (3) Metrics-row persistence ──────────────────────────────────────
        // Migrated in-memory DB — migration 013 relaxed the harness CHECK to accept
        // any [a-z0-9][a-z0-9_-]* slug, so "gemini" is a valid harness value.
        let db =
            crate::db::schema::Database::open_in_memory().expect("in-memory migrated DB must open");
        let new_row = crate::db::metrics::NewTokenUsage {
            ts: "2026-06-29T00:00:00Z".into(),
            source: "mcp_response".into(),
            harness: "gemini".into(),
            model: None,
            tool_name: None,
            session_id: None,
            collab_session_id: None,
            collab_phase: None,
            task_tag: Some("issue-155-e2e".into()),
            input_tokens: 42,
            output_tokens: 7,
            cache_creation_input_tokens: 0,
            cache_read_input_tokens: 0,
            estimated: false,
            chars: 0,
            cost_usd: None,
            map_status: None,
            turn_id: None,
            area: None,
            original_response_bytes: None,
            compacted_response_bytes: None,
        };
        let row_id = db
            .insert_token_usage(&new_row)
            .expect("gemini token_usage INSERT must succeed");
        assert!(row_id > 0, "rowid must be positive");

        let persisted = db
            .query_token_usage(&crate::db::metrics::TokenUsageQuery {
                task_tag: Some("issue-155-e2e".into()),
                ..Default::default()
            })
            .expect("query must succeed");
        assert_eq!(persisted.len(), 1, "exactly one row must persist");
        assert_eq!(
            persisted[0].harness, "gemini",
            "harness field must round-trip"
        );
        assert_eq!(persisted[0].input_tokens, 42);

        // ── (4) Launcher arg-build ────────────────────────────────────────────
        // launch_invocation is pub(crate) + #[cfg(test)] and reachable from any
        // test module within the same crate.
        let (bin, args) = crate::launcher::launch_invocation(&GEMINI_E2E, Some("do it"));
        assert_eq!(
            bin, "gemini",
            "launcher must derive binary from spec.binary"
        );
        assert_eq!(
            args,
            vec!["do it".to_string()],
            "prompt must become the single positional argv"
        );
    }

    // ---- #190 Task 11: grok/gemini registry rows + proxy-command helper ---

    #[test]
    fn registry_contains_grok_spec() {
        let spec = by_id("grok", REGISTRY).expect("grok must be in REGISTRY");
        assert_eq!(spec.display_name, "Grok");
        assert_eq!(spec.binary, "grok");
        assert_eq!(spec.rules_file, "GROK.md");
        assert_eq!(
            spec.rules_strategy,
            RulesStrategy::Import {
                directive: "@AGENTS.md"
            }
        );
        assert!(
            !spec.write_rules_default,
            "grok is scaffolding-only, not yet a default write-rules target"
        );
        assert_eq!(spec.client_info_aliases, &["grok"]);
        assert_eq!(spec.env_aliases, &["grok"]);
    }

    #[test]
    fn registry_contains_gemini_spec() {
        let spec = by_id("gemini", REGISTRY).expect("gemini must be in REGISTRY");
        assert_eq!(spec.display_name, "Gemini CLI");
        assert_eq!(spec.binary, "gemini");
        assert_eq!(spec.rules_file, "GEMINI.md");
        assert_eq!(
            spec.rules_strategy,
            RulesStrategy::Import {
                directive: "@AGENTS.md"
            }
        );
        assert!(
            !spec.write_rules_default,
            "gemini is scaffolding-only, not yet a default write-rules target"
        );
        assert_eq!(spec.client_info_aliases, &["gemini"]);
        assert_eq!(spec.env_aliases, &["gemini"]);
    }

    #[test]
    fn grok_and_gemini_ids_are_valid_harness_slugs() {
        // Guards against a typo'd/uppercase id shipping undetected, same as
        // `every_registry_id_is_a_valid_harness_slug` above but scoped to
        // just the two new rows for a focused failure message.
        assert!(HarnessId::validate("grok").is_ok());
        assert!(HarnessId::validate("gemini").is_ok());
    }

    fn test_config_for_proxy_helper(state_dir: &std::path::Path) -> crate::config::Config {
        crate::config::Config {
            db_path: state_dir.join("memory.sqlite3"),
            model_dir: state_dir.join("models"),
            model_dir_explicit: false,
            state_dir: state_dir.to_path_buf(),
            mcp_access_mode: crate::config::McpAccessMode::ReadOnly,
            embed_mode: crate::config::EmbedMode::Noop,
        }
    }

    #[test]
    fn proxy_command_args_matches_the_canonical_connect_invocation() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_for_proxy_helper(dir.path());

        let args = proxy_command_args("claude", &cfg);

        assert_eq!(
            args,
            vec![
                "serve".to_string(),
                "--connect".to_string(),
                cfg.daemon_socket_path().display().to_string(),
            ]
        );
    }

    #[test]
    fn proxy_command_args_identical_for_every_registry_harness() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_for_proxy_helper(dir.path());

        let mut results: Vec<Vec<String>> = REGISTRY
            .iter()
            .map(|spec| proxy_command_args(spec.id, &cfg))
            .collect();
        assert_eq!(results.len(), REGISTRY.len());

        let first = results.pop().expect("REGISTRY is non-empty");
        assert!(
            results.into_iter().all(|args| args == first),
            "every harness id must resolve to the exact same proxy command args"
        );
    }

    #[test]
    fn proxy_command_args_uses_the_default_socket_path() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = test_config_for_proxy_helper(dir.path());

        let args = proxy_command_args("gemini", &cfg);
        let socket_arg = args.last().expect("args must end with the socket path");
        assert_eq!(socket_arg, &cfg.daemon_socket_path().display().to_string());
        assert!(
            socket_arg.ends_with("daemon.sock"),
            "must be Config::daemon_socket_path's default, got: {socket_arg}"
        );
    }
}
