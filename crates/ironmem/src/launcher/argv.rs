//! Pure construction of the argument vector passed to the launched assistant.
//! Most harnesses (`claude`, `codex`, `grok`, `muse`) accept an optional initial
//! prompt as a single positional argument (`<bin> [prompt]`), while Antigravity
//! (`agy`) rejects bare positional prompts and expects `-i <prompt>`
//! (`--prompt-interactive`) to run an initial prompt interactively. Which form
//! a harness takes is its registry spec's `prompt_flag`, so no harness id is
//! named here.

use crate::harness::HarnessSpec;

pub(crate) fn build_args_for(spec: &HarnessSpec, prompt: Option<&str>) -> Vec<String> {
    match (prompt, spec.prompt_flag) {
        (Some(p), Some(flag)) => vec![flag.to_string(), p.to_string()],
        (Some(p), None) => vec![p.to_string()],
        (None, _) => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::{by_id, REGISTRY};

    fn spec(id: &str) -> &'static HarnessSpec {
        by_id(id, REGISTRY).expect("registry harness")
    }

    #[test]
    fn no_prompt_yields_empty_args() {
        assert!(build_args_for(spec("claude"), None).is_empty());
        assert!(build_args_for(spec("agy"), None).is_empty());
    }

    #[test]
    fn prompt_becomes_single_positional_for_positional_harnesses() {
        for id in ["claude", "codex", "grok", "muse"] {
            assert_eq!(
                build_args_for(spec(id), Some("fix the login bug")),
                vec!["fix the login bug"],
                "{id}"
            );
        }
    }

    #[test]
    fn prompt_uses_interactive_flag_for_agy() {
        assert_eq!(
            build_args_for(spec("agy"), Some("fix the login bug")),
            vec!["-i", "fix the login bug"]
        );
    }

    /// The flag comes from the spec, not from the harness id: a synthetic
    /// harness that declares one gets it, and an "agy"-named spec without
    /// one does not.
    #[test]
    fn prompt_flag_is_read_from_the_spec_not_the_id() {
        let flagged = HarnessSpec {
            id: "zeta",
            prompt_flag: Some("--prompt"),
            ..*spec("claude")
        };
        assert_eq!(build_args_for(&flagged, Some("go")), vec!["--prompt", "go"]);
        let renamed = HarnessSpec {
            prompt_flag: None,
            ..*spec("agy")
        };
        assert_eq!(build_args_for(&renamed, Some("go")), vec!["go"]);
    }

    #[test]
    fn empty_prompt_string_is_preserved() {
        // A caller that passes Some("") asked for an empty prompt; preserve it
        // rather than silently dropping — argv shape is the caller's contract.
        assert_eq!(build_args_for(spec("claude"), Some("")), vec![""]);
        assert_eq!(build_args_for(spec("agy"), Some("")), vec!["-i", ""]);
    }
}
