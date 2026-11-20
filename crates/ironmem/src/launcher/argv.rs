//! Pure construction of the argument vector passed to the launched assistant.
//! Most harnesses (`claude`, `codex`, `grok`, `muse`) accept an optional initial
//! prompt as a single positional argument (`<bin> [prompt]`), while Antigravity
//! (`agy`) rejects bare positional prompts and expects `-i <prompt>`
//! (`--prompt-interactive`) to run an initial prompt interactively.

pub(crate) fn build_args_for(harness_id: &str, prompt: Option<&str>) -> Vec<String> {
    match prompt {
        Some(p) if harness_id == "agy" => vec!["-i".to_string(), p.to_string()],
        Some(p) => vec![p.to_string()],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_prompt_yields_empty_args() {
        assert!(build_args_for("claude", None).is_empty());
        assert!(build_args_for("agy", None).is_empty());
    }

    #[test]
    fn prompt_becomes_single_positional_for_positional_harnesses() {
        assert_eq!(
            build_args_for("claude", Some("fix the login bug")),
            vec!["fix the login bug"]
        );
        assert_eq!(
            build_args_for("codex", Some("fix the login bug")),
            vec!["fix the login bug"]
        );
    }

    #[test]
    fn prompt_uses_interactive_flag_for_agy() {
        assert_eq!(
            build_args_for("agy", Some("fix the login bug")),
            vec!["-i", "fix the login bug"]
        );
    }

    #[test]
    fn empty_prompt_string_is_preserved() {
        // A caller that passes Some("") asked for an empty prompt; preserve it
        // rather than silently dropping — argv shape is the caller's contract.
        assert_eq!(build_args_for("claude", Some("")), vec![""]);
        assert_eq!(build_args_for("agy", Some("")), vec!["-i", ""]);
    }
}
