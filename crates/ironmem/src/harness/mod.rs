//! Harness registry — canonical descriptions of supported AI assistant harnesses.
//!
//! The `REGISTRY` constant is the single source of truth for harness identifiers,
//! binaries, rules files, client-info aliases, and capability flags.  All
//! lookup helpers take an explicit `registry: &[HarnessSpec]` slice so they
//! work on injected test slices without global-state mutation.

mod packaging;
pub use packaging::check_packaging_coverage;

/// How a harness encodes session transcripts (used by the abeval token parser).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
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
// Registry serialization helpers
// ---------------------------------------------------------------------------

/// Serialize the registry as pretty JSON (id, display_name, binary, rules_file,
/// write_rules_default, aliases, capability flags). Used by `ironmem harnesses
/// --format=json` and packaging drift-lint.
pub fn registry_json(registry: &[HarnessSpec]) -> Result<String, serde_json::Error> {
    serde_json::to_string_pretty(registry)
}

/// Human-readable one-line-per-harness listing.
///
/// Format: `{id}  {display_name}  rules={rules_file}  binary={binary}`
pub fn registry_text(registry: &[HarnessSpec]) -> String {
    registry
        .iter()
        .map(|s| {
            format!(
                "{}  {}  rules={}  binary={}\n",
                s.id, s.display_name, s.rules_file, s.binary
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

    // ---- registry_json / registry_text ------------------------------------

    #[test]
    fn registry_json_parses_as_two_entry_array() {
        let json = registry_json(REGISTRY).expect("serialization must succeed");
        let val: serde_json::Value =
            serde_json::from_str(&json).expect("output must be valid JSON");
        let arr = val.as_array().expect("top-level must be an array");
        assert_eq!(arr.len(), 2);
        let ids: Vec<&str> = arr.iter().filter_map(|e| e["id"].as_str()).collect();
        assert!(ids.contains(&"claude"), "must contain claude entry");
        assert!(ids.contains(&"codex"), "must contain codex entry");
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
                entry["transcript_parser"].is_string(),
                "transcript_parser must be a string"
            );
        }
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
}
