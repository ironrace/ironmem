//! Pure construction of the argument vector passed to the launched assistant.
//! Both `claude` and `codex` accept an optional initial prompt as a single
//! positional argument: `<bin> [prompt]`.

pub(crate) fn build_args(prompt: Option<&str>) -> Vec<String> {
    match prompt {
        Some(p) => vec![p.to_string()],
        None => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_prompt_yields_empty_args() {
        assert!(build_args(None).is_empty());
    }

    #[test]
    fn prompt_becomes_single_positional() {
        assert_eq!(
            build_args(Some("fix the login bug")),
            vec!["fix the login bug"]
        );
    }

    #[test]
    fn empty_prompt_string_is_still_a_positional() {
        // A caller that passes Some("") asked for an empty positional; preserve it
        // rather than silently dropping — argv shape is the caller's contract.
        assert_eq!(build_args(Some("")), vec![""]);
    }
}
