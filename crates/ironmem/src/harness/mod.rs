//! Harness registry — canonical descriptions of supported AI assistant harnesses.
//!
//! The `REGISTRY` constant is the single source of truth for harness identifiers,
//! binaries, rules files, client-info aliases, and capability flags.  All
//! lookup helpers take an explicit `registry: &[HarnessSpec]` slice so they
//! work on injected test slices without global-state mutation.

/// How a harness encodes session transcripts (used by the abeval token parser).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TranscriptParserKind {
    Claude,
    Codex,
    None,
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
    pub const fn new_unchecked(s: &'static str) -> Self {
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
#[derive(Debug, Clone, Copy)]
pub struct HarnessSpec {
    /// Canonical lowercase identifier (e.g. `"claude"`, `"codex"`).
    pub id: &'static str,
    /// Human-readable display name (e.g. `"Claude Code"`).
    pub display_name: &'static str,
    /// Executable name looked up on `PATH` (e.g. `"claude"`).
    pub binary: &'static str,
    /// Rules file written by `ironmem write-rules` for this harness.
    pub rules_file: &'static str,
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
        write_rules_default: true,
        client_info_aliases: &["codex"],
        env_aliases: &["codex"],
        additional_context_support: false,
        occupancy_support: true,
        transcript_parser: TranscriptParserKind::Codex,
    },
];

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
pub fn default_rules_targets(registry: &[HarnessSpec]) -> Vec<&'static str> {
    registry
        .iter()
        .filter(|s| s.write_rules_default)
        .map(|s| s.rules_file)
        .collect()
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
        let targets = default_rules_targets(REGISTRY);
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
        let targets = default_rules_targets(&reg);
        assert!(
            !targets.contains(&"GEMINI.md"),
            "GEMINI.md should be excluded"
        );
        assert_eq!(targets.len(), 2);
    }
}
